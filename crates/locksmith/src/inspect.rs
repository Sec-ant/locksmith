//! Read-only diagnostics for one complete project lockfile.
mod impact;

use crate::{
    Disposition, Lockfile, LockfileError, PeerRules, PeerStatus, Value, diagnostics, graph,
};
use node_semver::Version;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Read-only report for the project document of a validated lockfile.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    /// Counts for the recorded graph and the part reachable from workspaces.
    pub summary: InspectionSummary,
    /// Matching policy used for peer findings.
    pub peer_policy: crate::PeerVersionMatching,
    /// Peer findings grouped by requirement and outcome.
    pub peers: Vec<PeerGroup>,
    /// Package names with several locked sources, versions or snapshot instances.
    pub multiplicity: Vec<PackageInstances>,
    /// Recorded local links whose target has no matching importer.
    pub external_links: BTreeSet<ExternalLink>,
}

/// Sizes of the project graph before and after workspace reachability is considered.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectionSummary {
    /// Number of recorded importer entries.
    pub workspaces: usize,
    /// Number of package metadata records.
    pub packages: usize,
    /// Number of recorded installed instances.
    pub snapshots: usize,
    /// Number of instances reachable from at least one workspace.
    pub reachable_snapshots: usize,
}

/// A local link leading outside the graph described by the lockfile.
#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalLink {
    /// Section containing the dependency edge.
    pub source_kind: LinkSource,
    /// Importer path or snapshot ID containing the link.
    pub from: String,
    /// Dependency key used by the source.
    pub dependency: String,
    /// Normalized destination of the link, relative to the workspace root.
    pub target: String,
}

/// Kind of graph record containing a local link.
#[derive(Clone, Copy, Debug, PartialOrd, Ord, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkSource {
    /// A dependency declared by a workspace project.
    Importer,
    /// A dependency recorded for a package instance.
    Snapshot,
}

/// Locked releases and instances sharing a package name.
#[derive(Debug, Serialize)]
pub struct PackageInstances {
    /// Package name from the record or its locator.
    pub name: String,
    /// Distinct locked releases or sources for this name.
    pub releases: Vec<LockedRelease>,
}

/// One package metadata record and its installed instances.
#[derive(Debug, Serialize)]
pub struct LockedRelease {
    /// Complete key in the lockfile’s packages section.
    pub id: String,
    /// Semantic version of the release, when known.
    pub version: Option<String>,
    /// Number of snapshot records that refer to this release.
    pub snapshots: usize,
}

/// A peer contract and its evaluation, shared by one or more requester snapshots.
#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRequirement {
    /// Finding reported after applying workspace exceptions.
    pub status: PeerStatus,
    /// Requester package key without the snapshot’s peer suffix.
    pub requester: String,
    /// Dependency name under which the peer is requested.
    pub peer: String,
    /// Specifier declared by the requester.
    pub wanted: String,
    /// Package name used to search for compatible releases, accounting for aliases.
    pub provider: String,
    /// Semantic version of the bound provider, when known.
    pub resolved_version: Option<String>,
    /// Install-time acceptance under the selected policy.
    pub disposition: Disposition,
    /// Missing evidence for an indeterminate comparison.
    pub reason: Option<crate::UnknownReason>,
    /// Configured rule that matched the finding, when present.
    pub exception: Option<String>,
}

/// One requester instance and its recorded provider edge.
#[derive(Debug, Serialize)]
pub struct PeerBinding {
    /// Full ID of the requesting snapshot.
    pub snapshot: String,
    /// Locked dependency reference, or `None` for a missing peer.
    pub reference: Option<String>,
}

/// Occurrences of the same peer finding and the workspace projects affected.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerGroup {
    /// Peer declaration and result shared by the listed bindings.
    #[serde(flatten)]
    pub requirement: PeerRequirement,
    /// Requester snapshots exhibiting this finding.
    pub bindings: Vec<PeerBinding>,
    /// Known versions of the provider already in the lock that satisfy the declared range.
    pub matching_versions_in_lock: Vec<String>,
    /// Importers with a direct edge to one of the requester snapshots.
    pub direct_workspaces: Vec<String>,
    /// Importers that reach a requester through one or more recorded edges, including direct dependencies.
    pub reachable_workspaces: Vec<String>,
    /// Importer followed by package snapshots or linked importers, ending at the requester.
    pub example_path: Option<Vec<String>>,
}

/// Validate the graph and inspect its recorded peer contracts and package instances.
///
/// # Errors
/// Returns an error for an invalid graph or a lock without a project document.
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
