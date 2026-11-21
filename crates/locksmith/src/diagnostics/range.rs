//! Peer resolution in pnpm 9–11 uses Yarn's prerelease comparison. pnpm 11
//! adds scheme ranges. pnpm 12 also checks the release core in its resolver.
use super::{Disposition, PeerStatus, PeerVersionMatching, UnknownReason};
use nodejs_semver::{Range, Version};

pub(crate) struct PeerRange(Vec<ParsedRange>);

impl PeerRange {
    pub(crate) fn new(wanted: &str, mode: PeerVersionMatching) -> Self {
        use PeerVersionMatching::*;
        let policies: &[_] = match mode {
            Unspecified => &[ReleaseCompatible, Pnpm11, Pnpm12],
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

    pub(super) fn check(
        &self,
        version: &Version,
    ) -> Option<(PeerStatus, Disposition, Option<UnknownReason>)> {
        let mut outcomes = self.0.iter().map(|r| r.check(version));
        let first = outcomes.next().expect("at least one resolver policy");
        if outcomes.all(|outcome| outcome == first) {
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
            return self.arms.iter().any(|arm| arm.range.satisfies(version));
        }
        let core = Version::new(
            version.major(),
            version.minor(),
            version.patch(),
            vec![],
            vec![],
        );
        self.arms.iter().any(|arm| {
            arm.partial_ceiling
                .as_ref()
                .is_none_or(|upper| core < *upper)
                && arm.range.satisfies_with_prerelease(version, true)
                || (self.mode == PeerVersionMatching::Pnpm12
                    || !arm.open_prerelease_bounds.contains(version))
                    && arm.range.satisfies(&core)
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

struct Alternative {
    range: Range,
    partial_ceiling: Option<Version>,
    open_prerelease_bounds: Vec<Version>,
}

impl Alternative {
    fn parse(text: &str) -> Option<Self> {
        let range = Range::parse(text).ok()?;
        let mut partial_ceiling: Option<Version> = None;
        let mut open_prerelease_bounds = Vec::new();
        let mut tokens = text.split_whitespace();
        while let Some(token) = tokens.next() {
            let Some((operator, body)) = token.split_at_checked(1) else {
                continue;
            };
            if !matches!(operator, "<" | ">") {
                continue;
            }
            let body = if body.is_empty() {
                tokens.next().unwrap_or("")
            } else {
                body
            };
            if let Ok(version) = Version::parse(body) {
                // Yarn retries a prerelease as its release core, but an exclusive
                // comparator still excludes the exact version it names.
                if version.is_prerelease() {
                    open_prerelease_bounds.push(version);
                }
            } else if operator == "<" && !body.is_empty() && !body.starts_with('=') {
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
            open_prerelease_bounds,
        })
    }
}
