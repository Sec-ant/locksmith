//! Peer ranges use npm comparator sets. pnpm policies also allow prereleases
//! against their release core and, from pnpm 11, ranges inside scheme specs.
use node_semver::{Range, Version};
use serde::Serialize;
use std::borrow::Cow;

/// Package-manager policy for comparing a peer declaration with its provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Hash)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub enum PeerVersionMatching {
    /// Standard npm semver, including its prerelease filtering.
    #[default]
    Semver,
    /// Yarn-compatible peer comparison used by the pnpm 10 resolver.
    #[serde(rename = "pnpm10")]
    Pnpm10,
    /// pnpm 9 applies configured exceptions to displayed warnings only.
    Pnpm9,
    /// JS resolver with scheme ranges.
    Pnpm11,
    /// Native resolver and install-time peer check.
    Pnpm12,
    /// Report a definite result when all supported resolvers agree.
    Unspecified,
}
impl PeerVersionMatching {
    /// Select the matching policy for a pnpm release, or compare across supported policies when unspecified.
    #[must_use]
    pub fn for_pnpm(version: Option<&node_semver::Version>) -> Self {
        match version.map(|v| v.major()) {
            Some(9) => Self::Pnpm9,
            Some(10) => Self::Pnpm10,
            Some(11) => Self::Pnpm11,
            Some(12) => Self::Pnpm12,
            _ => Self::Unspecified,
        }
    }
}

/// What the lock and configured exceptions say about a peer binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Hash)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub enum PeerStatus {
    /// The requester has no binding for a required peer.
    Missing,
    /// The provider version falls outside the requested range.
    OutOfRange,
    /// Available evidence does not determine whether the binding satisfies the declaration.
    Unknown,
    /// The peer matches an explicit workspace exception. See [`Disposition`] for install-time acceptance.
    Allowed,
}

/// Whether the selected peer policy accepts this binding. Unknown evidence
/// (for example, an external link without its manifest) stays explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Hash)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub enum Disposition {
    /// The selected package-manager policy accepts the binding.
    Accepted,
    /// The selected policy reports an unmet peer.
    Rejected,
    /// The outcome depends on evidence or a package-manager version the caller did not supply.
    Unknown,
}

/// Evidence missing for a definitive version comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Hash)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub enum UnknownReason {
    /// The linked or otherwise opaque provider has no known version.
    ProviderVersion,
    /// The requested specifier cannot be compared as a version range.
    OpaqueSpecifier,
    /// Supported pnpm policies disagree and no specific policy was selected.
    PackageManager,
}

pub(crate) struct PeerRange(Vec<ParsedRange>);

impl PeerRange {
    pub(crate) fn new(wanted: &str, mode: PeerVersionMatching) -> Self {
        use PeerVersionMatching::*;
        let policies: &[_] = match mode {
            Unspecified => &[Pnpm10, Pnpm11],
            _ => std::slice::from_ref(&mode),
        };
        Self(
            policies
                .iter()
                .map(|&mode| ParsedRange::new(wanted, mode))
                .collect(),
        )
    }

    pub(crate) fn accepts(&self, version: &Version) -> bool {
        self.0.iter().all(|r| r.accepts(version))
    }

    pub(crate) fn check(
        &self,
        version: &Version,
    ) -> Option<(PeerStatus, Disposition, Option<UnknownReason>)> {
        let mut outcomes = self.0.iter().map(|r| r.check(version));
        let first = outcomes.next().expect("at least one resolver policy");
        if outcomes.all(|other| other == first) {
            first
        } else {
            Some((
                PeerStatus::Unknown,
                Disposition::Unknown,
                Some(UnknownReason::PackageManager),
            ))
        }
    }
}

struct ParsedRange {
    arms: Vec<Alternative>,
    opaque: bool,
    wildcard: bool,
    mode: PeerVersionMatching,
}

impl ParsedRange {
    fn new(wanted: &str, mode: PeerVersionMatching) -> Self {
        let schemes = matches!(
            mode,
            PeerVersionMatching::Pnpm11 | PeerVersionMatching::Pnpm12
        );
        // The older JS resolver strips exactly one leading workspace: prefix.
        let wanted = if schemes {
            wanted
        } else {
            wanted.strip_prefix("workspace:").unwrap_or(wanted)
        };
        let mut result = Self {
            arms: vec![],
            opaque: false,
            wildcard: false,
            mode,
        };
        for arm in wanted.split("||").map(str::trim) {
            let workspace = schemes.then(|| arm.strip_prefix("workspace:")).flatten();
            let spec = workspace.unwrap_or(arm);
            let parsed = Alternative::parse(spec).or_else(|| {
                if !schemes {
                    return None;
                }
                if workspace.is_some() {
                    return Alternative::parse("*");
                }
                let (scheme, body) = spec.split_once(':')?;
                if scheme.is_empty() || scheme == "catalog" {
                    return None;
                }
                Alternative::parse(body).or_else(|| Alternative::parse(body.rsplit_once('@')?.1))
            });
            if let Some(range) = parsed {
                result.arms.push(range);
            } else {
                result.opaque = true;
                result.wildcard |= schemes && !arm.starts_with("catalog:");
            }
        }
        result
    }

    fn accepts(&self, version: &Version) -> bool {
        if !version.is_prerelease() || self.mode == PeerVersionMatching::Semver {
            return self
                .arms
                .iter()
                .any(|arm| arm.range.as_ref().is_some_and(|r| r.satisfies(version)));
        }
        let core = Version::from((version.major(), version.minor(), version.patch()));
        self.arms.iter().any(|arm| {
            let Some(range) = &arm.range else {
                return false;
            };
            arm.partial_ceiling
                .as_ref()
                .is_none_or(|upper| core < *upper)
                && range.satisfies_with_prerelease(version, true)
                || !arm.explicit_prerelease && range.satisfies(&core)
        })
    }

    fn check(&self, version: &Version) -> Option<(PeerStatus, Disposition, Option<UnknownReason>)> {
        if self.accepts(version) {
            return None;
        }
        if self.opaque {
            let disposition = if self.wildcard {
                Disposition::Accepted
            } else if self.mode == PeerVersionMatching::Semver {
                Disposition::Unknown
            } else {
                Disposition::Rejected
            };
            return Some((
                PeerStatus::Unknown,
                disposition,
                Some(UnknownReason::OpaqueSpecifier),
            ));
        }
        Some((PeerStatus::OutOfRange, Disposition::Rejected, None))
    }
}

pub(crate) fn satisfies_semver(version: &Version, wanted: &str) -> bool {
    wanted
        .split("||")
        .map(str::trim)
        .filter_map(Alternative::parse)
        .filter_map(|arm| arm.range)
        .any(|range| range.satisfies(version))
}

fn comparator(text: &str) -> &str {
    if text == ">*" { "<0.0.0-0" } else { text }
}

struct Alternative {
    // None denotes a valid arm with an empty intersection.
    range: Option<Range>,
    partial_ceiling: Option<Version>,
    explicit_prerelease: bool,
}

impl Alternative {
    fn parse(text: &str) -> Option<Self> {
        let whole = comparator(text);
        let mut range = Some(Range::parse(whole).ok()?);
        let mut partial_ceiling: Option<Version> = None;
        let mut explicit_prerelease = false;
        let mut tokens = text.split_whitespace().peekable();
        while let Some(token) = tokens.next() {
            // Whitespace is legal between an operator and its operand. A
            // hyphen range is one comparator even though it spans three words.
            let term: Cow<'_, str> =
                if matches!(token, ">" | ">=" | "<" | "<=" | "=" | "~" | "~>" | "^")
                    && tokens.peek().is_some()
                {
                    format!("{token}{}", tokens.next().unwrap()).into()
                } else if tokens.next_if_eq(&"-").is_some() {
                    format!("{token} - {}", tokens.next().unwrap_or("")).into()
                } else {
                    token.into()
                };
            let normalized = comparator(&term);
            if normalized != whole
                && let Ok(part) = Range::parse(normalized)
            {
                range = range.and_then(|r| r.intersect(&part));
            }
            if term.contains('-') {
                explicit_prerelease |= term.split_whitespace().any(|part| {
                    Version::parse(part.trim_start_matches(['<', '>', '=', '~', '^']))
                        .is_ok_and(|v| v.is_prerelease())
                });
            }
            if let Some(body) = term.strip_prefix('<')
                && !body.is_empty()
                && !body.starts_with('=')
                && Version::parse(body).is_err()
            {
                // npm expands <2 (and <2.x) to <2.0.0-0, excluding 2.0.0-beta.
                // An explicit <2.0.0 includes prereleases of 2.0.0.
                if let Some(minimum) = Range::parse(body).ok().and_then(|r| r.min_version())
                    && partial_ceiling.as_ref().is_none_or(|v| minimum < *v)
                {
                    partial_ceiling = Some(minimum);
                }
            }
        }
        Some(Self {
            range,
            partial_ceiling,
            explicit_prerelease,
        })
    }
}
