//! Pure peer diagnostics, separate from graph construction and acceptance.

use crate::{Lockfile, LockfileError, Value, graph, manifest, peer};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PeerVersionMatching {
    /// Standard npm semver, including its prerelease filtering.
    #[default]
    Semver,
    /// Compare prereleases and release cores, as used by pnpm 9–11 peers.
    ReleaseCompatible,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerRules {
    pub version_matching: PeerVersionMatching,
    pub ignore_missing: Vec<String>,
    pub allow_any: Vec<String>,
    /// Selector/range pairs in declaration order.
    pub allowed_versions: Vec<(String, String)>,
}
impl PeerRules {
    /// Decode pnpm peer exceptions with an explicitly chosen matching rule.
    /// This does not read packageManager or strictPeerDependencies.
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
    pub snapshot: String,
    pub peer: String,
    pub wanted: String,
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
    let Some(document) = lockfile.project() else {
        return Vec::new();
    };
    let mut issues = BTreeSet::new();
    for (key, body) in graph::section(document, "snapshots").into_iter().flatten() {
        let package = document
            .get("packages")
            .and_then(|v| v.get(graph::package_key(key)));
        for (name, range) in package
            .and_then(|p| p.get("peerDependencies"))
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let Some(wanted) = range.as_str() else {
                continue;
            };
            let resolved = ["dependencies", "optionalDependencies"]
                .into_iter()
                .find_map(|g| body.get(g)?.get(name)?.as_str());
            let optional = package
                .and_then(|p| p.get("peerDependenciesMeta"))
                .and_then(|v| v.get(name))
                .and_then(|v| v.get("optional"))
                .and_then(Value::as_bool)
                == Some(true);
            if resolved.is_none() && optional {
                continue;
            }
            let resolved_version = resolved.and_then(|r| {
                if let Some(path) = r.strip_prefix("link:") {
                    workspace_manifests?
                        .get(&graph::linked_importer(".", path))?
                        .get("version")?
                        .as_str()
                } else {
                    Some(graph::package_key(peer::canonical_binding(name, r).1))
                }
            });
            let satisfied = resolved_version
                .and_then(|v| v.parse::<node_semver::Version>().ok())
                .is_some_and(|v| {
                    if rules.version_matching == PeerVersionMatching::ReleaseCompatible {
                        peer::satisfies_peer(&v, wanted)
                    } else {
                        wanted
                            .strip_prefix("workspace:")
                            .unwrap_or(wanted)
                            .parse::<node_semver::Range>()
                            .is_ok_and(|r| v.satisfies(&r))
                    }
                });
            if !satisfied && !allowed_peer(rules, key, name, resolved_version.or(resolved)) {
                issues.insert(PeerIssue {
                    snapshot: key.clone(),
                    peer: name.clone(),
                    wanted: wanted.into(),
                    resolved: resolved.map(str::to_owned),
                });
            }
        }
    }
    issues.into_iter().collect()
}

fn allowed_peer(rules: &PeerRules, key: &str, name: &str, resolved: Option<&str>) -> bool {
    let Some(resolved) = resolved else {
        return matches_patterns(&rules.ignore_missing, name);
    };
    if matches_patterns(&rules.allow_any, name) {
        return true;
    }
    let bare = graph::package_key(resolved);
    let version = bare.rsplit_once('@').map_or(bare, |(_, v)| v);
    let Ok(version) = version.parse::<node_semver::Version>() else {
        return false;
    };
    let Some((parent, parent_version)) = graph::package_key(key).rsplit_once('@') else {
        return false;
    };
    let accepts = |range: &String| {
        range
            .parse::<node_semver::Range>()
            .is_ok_and(|range| version.satisfies(&range))
    };
    let global = rules.allowed_versions.iter().rev().find(|(rule, _)| {
        let (selector, peer, _) = manifest::override_selector(rule);
        selector.is_none() && peer == name
    });
    if global.is_some_and(|(_, range)| accepts(range)) {
        return true;
    }
    rules
        .allowed_versions
        .iter()
        .rev()
        .find(|(rule, _)| {
            let (selector, peer, _) = manifest::override_selector(rule);
            peer == name
                && selector.is_some_and(|s| manifest::parent_matches(s, parent, parent_version))
        })
        .is_some_and(|(_, range)| accepts(range))
}

// pnpm's matcher treats only `*` as a wildcard and applies negations in order.
fn matches_patterns(patterns: &[String], name: &str) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let mut matched = patterns.iter().all(|p| p.starts_with('!'));
    for pattern in patterns.iter().map(String::as_str) {
        let (negative, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern), |p| (true, p));
        if wildcard(pattern, name) {
            matched = !negative;
        }
    }
    matched
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().expect("one split part");
    let Some(mut rest) = value.strip_prefix(first) else {
        return false;
    };
    let mut parts = parts.peekable();
    if parts.peek().is_none() {
        return rest.is_empty();
    }
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        let Some(offset) = rest.find(part) else {
            return false;
        };
        rest = &rest[offset + part.len()..];
    }
    true
}
