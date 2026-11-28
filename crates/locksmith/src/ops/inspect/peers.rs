//! Pure peer diagnostics, separate from graph construction and acceptance.

use crate::peer::range;
use crate::{Lockfile, LockfileError, PeerIssue, Value, model::graph, model::pin, model::schema};
use node_semver::Version;
pub(crate) use range::PeerRange;
pub use range::{Disposition, PeerStatus, PeerVersionMatching, UnknownReason};
use std::collections::BTreeMap;

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
    /// This does not read `packageManager` or `strictPeerDependencies`.
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
            ignore_missing: crate::model::config::decode(rules, "ignoreMissing")?
                .unwrap_or_default(),
            allow_any: crate::model::config::decode(rules, "allowAny")?.unwrap_or_default(),
            allowed_versions: crate::model::config::string_rules(rules, "allowedVersions")?
                .unwrap_or_default(),
        })
    }
}

/// Inspect peer requirements without changing the lockfile or accepting/rejecting
/// it. A lockfile containing only an environment document has no workspace peers.
/// Workspace manifests, keyed by importer path, supply versions for linked peers.
/// Without that data, linked peer versions remain unresolved diagnostics.
#[must_use]
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
            let Some(wanted) = declaration.as_str() else {
                continue;
            };
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
            let (mut status, mut disposition, mut reason) = if binding.is_none() {
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
                if disposition == Disposition::Accepted {
                    reason = None;
                } else if rules.version_matching == PeerVersionMatching::Unspecified {
                    reason.get_or_insert(UnknownReason::PackageManager);
                }
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
        return crate::model::config::matches_patterns(
            rules.ignore_missing.iter().map(String::as_str),
            name,
        )
        .then(|| "ignoreMissing".into());
    }
    if crate::model::config::matches_patterns(rules.allow_any.iter().map(String::as_str), name) {
        return Some("allowAny".into());
    }
    let version = version?;
    let parent_version = parent_version.and_then(|v| Version::parse(v).ok());
    let mut global = Vec::new();
    let mut scoped = Vec::new();
    for (rule, range) in &rules.allowed_versions {
        let (selector, peer, peer_range) = schema::override_selector(rule);
        if peer != name {
            continue;
        }
        match selector {
            None => {
                if peer_range.is_none() {
                    global.push((rule, range));
                }
            }
            Some(s) => {
                let (name, wanted) = pin::selector(s);
                if name == parent
                    && wanted.is_none_or(|r| {
                        parent_version
                            .as_ref()
                            .is_some_and(|v| range::satisfies_semver(v, r))
                    })
                    && peer_range.is_none_or(|r| range::satisfies_semver(version, r))
                {
                    scoped.push((rule, range));
                }
            }
        }
    }
    global
        .into_iter()
        .chain(scoped)
        .find(|(_, r)| range::satisfies_semver(version, r))
        .map(|(rule, _)| format!("allowedVersions/{rule}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentKind;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn inspect_peers_skips_non_string_peer_declarations_without_panicking() {
        let doc = json!({
            "lockfileVersion": "9.0",
            "importers": {".": {"dependencies": {"plugin": {"specifier": "1.0.0", "version": "1.0.0"}}}},
            "packages": {"plugin@1.0.0": {"resolution": {}, "peerDependencies": {"host": ["^1"]}}},
            "snapshots": {"plugin@1.0.0": {}}
        });
        let lock = Lockfile::with_documents(
            BTreeMap::from([(DocumentKind::Project, Arc::new(doc))]),
            &[],
        );
        assert!(inspect_peers(&lock, &PeerRules::default(), None).is_empty());
    }
}
