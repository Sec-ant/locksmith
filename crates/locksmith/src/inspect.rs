//! Read-only diagnostics for one complete project lockfile.
mod impact;

use crate::{
    Disposition, Lockfile, LockfileError, PeerRules, PeerStatus, Value, diagnostics, graph,
};
use nodejs_semver::Version;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub summary: InspectionSummary,
    pub peer_policy: crate::PeerVersionMatching,
    pub peers: Vec<PeerGroup>,
    /// Package names with several locked sources, versions or snapshot instances.
    pub multiplicity: Vec<PackageInstances>,
    pub external_links: BTreeSet<ExternalLink>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectionSummary {
    pub workspaces: usize,
    pub packages: usize,
    pub snapshots: usize,
    pub reachable_snapshots: usize,
}

#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalLink {
    pub source_kind: LinkSource,
    pub from: String,
    pub dependency: String,
    pub target: String,
}

#[derive(Clone, Copy, Debug, PartialOrd, Ord, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkSource {
    Importer,
    Snapshot,
}

#[derive(Debug, Serialize)]
pub struct PackageInstances {
    pub name: String,
    pub releases: Vec<LockedRelease>,
}

#[derive(Debug, Serialize)]
pub struct LockedRelease {
    pub id: String,
    pub version: Option<String>,
    pub snapshots: usize,
}

#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRequirement {
    pub status: PeerStatus,
    pub requester: String,
    pub peer: String,
    pub wanted: String,
    pub provider: String,
    pub resolved_version: Option<String>,
    pub disposition: Disposition,
    pub reason: Option<crate::UnknownReason>,
    pub exception: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PeerBinding {
    pub snapshot: String,
    pub reference: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerGroup {
    #[serde(flatten)]
    pub requirement: PeerRequirement,
    pub bindings: Vec<PeerBinding>,
    pub matching_versions_in_lock: Vec<String>,
    pub direct_workspaces: Vec<String>,
    pub reachable_workspaces: Vec<String>,
    /// Importer followed by package snapshots or linked importers, ending at the requester.
    pub example_path: Option<Vec<String>>,
}

/// Validate the graph and inspect its recorded peer contracts and package instances.
pub fn inspect(
    lock: &Lockfile,
    rules: &PeerRules,
    manifests: Option<&BTreeMap<String, Value>>,
) -> Result<Inspection, LockfileError> {
    lock.validate()?;
    let doc = lock
        .project()
        .ok_or_else(|| LockfileError::InvalidDocument("missing project document".into()))?;
    let mut counts = BTreeMap::new();
    for (id, _) in graph::section(doc, "snapshots").into_iter().flatten() {
        *counts.entry(graph::package_key(id)).or_insert(0usize) += 1;
    }
    let mut versions = BTreeMap::<&str, BTreeSet<Version>>::new();
    let mut releases = BTreeMap::<&str, Vec<LockedRelease>>::new();
    for (id, package) in graph::section(doc, "packages").into_iter().flatten() {
        let (name, v) = diagnostics::package_identity(id, package);
        let version = v.and_then(|s| Version::parse(s).ok());
        let text = version.as_ref().map(ToString::to_string);
        if let Some(v) = version {
            versions.entry(name).or_default().insert(v);
        }
        releases.entry(name).or_default().push(LockedRelease {
            id: id.clone(),
            version: text,
            snapshots: counts.get(id.as_str()).copied().unwrap_or(0),
        });
    }
    let multiplicity = releases
        .into_iter()
        .filter(|(_, v)| v.len() > 1 || v[0].snapshots > 1)
        .map(|(name, mut releases)| {
            releases.sort_by(|a, b| a.id.cmp(&b.id));
            PackageInstances {
                name: name.to_owned(),
                releases,
            }
        })
        .collect();
    let mut groups = BTreeMap::<PeerRequirement, Vec<PeerBinding>>::new();
    for record in diagnostics::scan(lock, rules, manifests) {
        let issue = record.issue;
        let requirement = PeerRequirement {
            status: record.status,
            requester: graph::package_key(&issue.snapshot).to_owned(),
            peer: issue.peer,
            wanted: issue.wanted,
            provider: record.provider,
            resolved_version: record.version,
            disposition: record.disposition,
            reason: record.reason,
            exception: record.exception,
        };
        groups.entry(requirement).or_default().push(PeerBinding {
            snapshot: issue.snapshot,
            reference: issue.resolved,
        });
    }
    let mut peers: Vec<_> = groups
        .into_iter()
        .map(|(requirement, mut bindings)| {
            bindings.sort_by(|a, b| a.snapshot.cmp(&b.snapshot));
            let range = diagnostics::PeerRange::new(&requirement.wanted, rules.version_matching);
            let matching_versions_in_lock = versions
                .get(requirement.provider.as_str())
                .into_iter()
                .flatten()
                .filter(|v| range.accepts(v))
                .map(ToString::to_string)
                .collect();
            PeerGroup {
                requirement,
                bindings,
                matching_versions_in_lock,
                direct_workspaces: vec![],
                reachable_workspaces: vec![],
                example_path: None,
            }
        })
        .collect();
    let impact = impact::Impact::build(doc)?;
    let reachable_snapshots = impact.attribute(&mut peers);
    peers.sort_by(|a, b| {
        a.requirement
            .status
            .cmp(&b.requirement.status)
            .then_with(|| {
                b.reachable_workspaces
                    .len()
                    .cmp(&a.reachable_workspaces.len())
            })
            .then_with(|| a.requirement.cmp(&b.requirement))
    });
    let len = |section| graph::section(doc, section).map_or(0, serde_json::Map::len);
    Ok(Inspection {
        peer_policy: rules.version_matching,
        summary: InspectionSummary {
            workspaces: len("importers"),
            packages: len("packages"),
            snapshots: len("snapshots"),
            reachable_snapshots,
        },
        peers,
        multiplicity,
        external_links: impact.external_links,
    })
}
