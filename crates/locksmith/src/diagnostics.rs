//! Pure peer diagnostics, separate from graph construction and acceptance.

mod range;

use crate::{Lockfile, LockfileError, Value, graph, manifest, pin};
use node_semver::Version;
pub(crate) use range::PeerRange;
use serde::Serialize;
use std::collections::BTreeMap;

/// Package-manager policy for comparing a peer declaration with its provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerVersionMatching {
    /// Standard npm semver, including its prerelease filtering.
    #[default]
    Semver,
    /// Yarn-compatible peer comparison used by the pnpm 10 resolver.
    #[serde(rename = "pnpm10")]
    ReleaseCompatible,
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
    pub fn for_pnpm(version: Option<&node_semver::Version>) -> Self {
        match version.map(|v| v.major()) {
            Some(9) => Self::Pnpm9,
            Some(10) => Self::ReleaseCompatible,
            Some(11) => Self::Pnpm11,
            Some(12) => Self::Pnpm12,
            _ => Self::Unspecified,
        }
    }
}

/// Version matching and workspace exceptions used to evaluate peer requirements.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerRules {
    /// Package-manager behavior used for ranges and exceptions.
    pub version_matching: PeerVersionMatching,
    /// Peer name patterns allowed to have no provider.
    pub ignore_missing: Vec<String>,
    /// Peer name patterns allowed to use any installed version.
    pub allow_any: Vec<String>,
    /// Selector/range pairs in declaration order.
    pub allowed_versions: Vec<(String, String)>,
}
impl PeerRules {
    /// Decode pnpm peer exceptions with an explicitly chosen matching rule.
    /// This does not read packageManager or strictPeerDependencies.
    ///
    /// # Errors
    /// Returns an error when the peer rules or their pattern lists have an invalid shape.
    pub fn from_pnpm(
        configuration: &Value,
        version_matching: PeerVersionMatching,
    ) -> Result<Self, LockfileError> {
        let Some(rules) = configuration.get("peerDependencyRules") else {
            return Ok(Self {
                version_matching,
                ..Self::default()
            });
        };
        if !rules.is_object() {
            return Err(LockfileError::InvalidDocument(
                "peerDependencyRules must be an object".into(),
            ));
        }
        Ok(Self {
            version_matching,
            ignore_missing: crate::config::decode(rules, "ignoreMissing")?.unwrap_or_default(),
            allow_any: crate::config::decode(rules, "allowAny")?.unwrap_or_default(),
            allowed_versions: crate::config::string_rules(rules, "allowedVersions")?
                .unwrap_or_default(),
        })
    }
}

/// An unmet peer requirement. The caller decides whether this rejects a result.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeerIssue {
    /// Requester snapshot ID, including its peer suffix.
    pub snapshot: String,
    /// Dependency name from the requesting package’s peer declarations.
    pub peer: String,
    /// Declared peer range or specifier.
    pub wanted: String,
    /// Locked provider reference, or `None` when the peer has no binding.
    pub resolved: Option<String>,
}

/// Inspect peer requirements without changing the lockfile or accepting/rejecting
/// it. A lockfile containing only an environment document has no workspace peers.
/// Workspace manifests, keyed by importer path, supply versions for linked peers.
/// Without that data, linked peer versions remain unresolved diagnostics.
pub fn inspect_peers(
    lockfile: &Lockfile,
    rules: &PeerRules,
    workspace_manifests: Option<&BTreeMap<String, Value>>,
) -> Vec<PeerIssue> {
    let mut issues: Vec<_> = scan(lockfile, rules, workspace_manifests)
        .into_iter()
        .filter(|record| record.disposition != Disposition::Accepted)
        .map(|record| record.issue)
        .collect();
    issues.sort();
    issues
}

/// What the lock and configured exceptions say about a peer binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UnknownReason {
    /// The linked or otherwise opaque provider has no known version.
    ProviderVersion,
    /// The requested specifier cannot be compared as a version range.
    OpaqueSpecifier,
    /// Supported pnpm policies disagree and no specific policy was selected.
    PackageManager,
}

pub(crate) struct Record {
    pub issue: PeerIssue,
    pub status: PeerStatus,
    pub reason: Option<UnknownReason>,
    pub disposition: Disposition,
    pub provider: String,
    pub version: Option<String>,
    pub exception: Option<String>,
}

pub(crate) fn package_identity<'a>(key: &'a str, record: &'a Value) -> (&'a str, Option<&'a str>) {
    let (name, locator) = pin::selector(key);
    (
        record["name"].as_str().unwrap_or(name),
        record["version"].as_str().or(locator),
    )
}

pub(crate) fn scan(
    lock: &Lockfile,
    rules: &PeerRules,
    manifests: Option<&BTreeMap<String, Value>>,
) -> Vec<Record> {
    let Some(doc) = lock.project() else {
        return Vec::new();
    };
    let links = graph::LinkedProjects::new(doc.get("importers"));
    let packages = graph::section(doc, "packages");
    let snapshots = graph::section(doc, "snapshots");
    let mut ranges = BTreeMap::new();
    let mut records = Vec::new();
    for (key, body) in snapshots.into_iter().flatten() {
        let Some(package) = packages.and_then(|p| p.get(pin::package_key(key))) else {
            continue;
        };
        for (name, declaration) in graph::section(package, "peerDependencies")
            .into_iter()
            .flatten()
        {
            let wanted = declaration.as_str().expect("validated peer declaration");
            let binding = graph::dependency(body, name).map(|(_, r)| r);
            if binding.is_none() && package["peerDependenciesMeta"][name]["optional"] == true {
                continue;
            }
            let (provider, version) = match binding {
                Some(pin) if pin.starts_with("link:") => {
                    let id = links.resolve(".", &pin[5..]);
                    let target = manifests.and_then(|m| m.get(&id));
                    (
                        target.and_then(|m| m["name"].as_str()).unwrap_or(name),
                        target.and_then(|m| m["version"].as_str()),
                    )
                }
                Some(pin) => {
                    let target = graph::snapshot_key(name, pin, snapshots);
                    packages
                        .and_then(|p| p.get_key_value(pin::package_key(&target)))
                        .map(|(id, record)| package_identity(id, record))
                        .unwrap_or((name, None))
                }
                None => (name.as_str(), None),
            };
            let parsed = version.and_then(|v| Version::parse(v).ok());
            let (mut status, mut disposition, reason) = if binding.is_none() {
                (PeerStatus::Missing, Disposition::Rejected, None)
            } else if let Some(v) = &parsed {
                let range = ranges
                    .entry(wanted)
                    .or_insert_with(|| PeerRange::new(wanted, rules.version_matching));
                let Some(outcome) = range.check(v) else {
                    continue;
                };
                outcome
            } else {
                (
                    PeerStatus::Unknown,
                    Disposition::Unknown,
                    Some(UnknownReason::ProviderVersion),
                )
            };
            let exception = allowed_peer(
                rules,
                package_identity(pin::package_key(key), package),
                name,
                binding,
                parsed.as_ref(),
            );
            if exception.is_some() {
                status = PeerStatus::Allowed;
                disposition = match rules.version_matching {
                    PeerVersionMatching::Pnpm9 => disposition,
                    PeerVersionMatching::Unspecified if disposition != Disposition::Accepted => {
                        Disposition::Unknown
                    }
                    _ => Disposition::Accepted,
                };
            }
            records.push(Record {
                issue: PeerIssue {
                    snapshot: key.clone(),
                    peer: name.clone(),
                    wanted: wanted.into(),
                    resolved: binding.map(str::to_owned),
                },
                status,
                disposition,
                reason,
                provider: provider.to_owned(),
                version: parsed.map(|v| v.to_string()),
                exception,
            });
        }
    }
    records
}

fn allowed_peer(
    rules: &PeerRules,
    (parent, parent_version): (&str, Option<&str>),
    name: &str,
    binding: Option<&str>,
    version: Option<&Version>,
) -> Option<String> {
    if binding.is_none() {
        return crate::config::matches_patterns(
            rules.ignore_missing.iter().map(String::as_str),
            name,
        )
        .then(|| "ignoreMissing".into());
    }
    if crate::config::matches_patterns(rules.allow_any.iter().map(String::as_str), name) {
        return Some("allowAny".into());
    }
    let version = version?;
    let parent_version = parent_version.and_then(|v| Version::parse(v).ok());
    let mut global = None;
    let mut scoped = None;
    for (rule, range) in &rules.allowed_versions {
        let (selector, peer, _) = manifest::override_selector(rule);
        if peer != name {
            continue;
        }
        match selector {
            None => global = Some((rule, range)),
            Some(s) => {
                let (name, wanted) = pin::selector(s);
                if name == parent
                    && wanted.is_none_or(|r| {
                        parent_version
                            .as_ref()
                            .is_some_and(|v| range::satisfies_semver(v, r))
                    })
                {
                    scoped = Some((rule, range));
                }
            }
        }
    }
    [global, scoped]
        .into_iter()
        .flatten()
        .find(|(_, r)| range::satisfies_semver(version, r))
        .map(|(rule, _)| format!("allowedVersions/{rule}"))
}
