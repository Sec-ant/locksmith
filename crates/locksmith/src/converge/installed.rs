//! Keep pnpm's complete peer assignments when mixing two otherwise usable
//! graphs would give one instance incompatible contents.
use std::collections::{BTreeSet, HashSet};

use super::{ChangeSet, ManifestChange, assembly};
use crate::{
    DocumentKind, Lockfile, LockfileError, Value, WorkspaceConfig, catalog, config, graph,
    manifest, patch, peer, pin, reconcile, schema,
};

pub(super) fn converge(
    baseline: &Lockfile,
    candidate: &Lockfile,
    inputs: ChangeSet<'_>,
    before: &WorkspaceConfig,
    after: &WorkspaceConfig,
) -> Result<Lockfile, LockfileError> {
    baseline.validate_with_config(Some(before))?;
    candidate.validate_with_config(Some(after))?;
    let base = baseline.project().expect("workspace project checked");
    let donor = candidate.project().expect("workspace project checked");
    check_manifests([base, donor], inputs, before, after)?;
    let policy = config::ResolutionCompatibility::new(&before.resolution, &after.resolution, base);
    let overrides = super::workspace::override_names([base, donor]);
    super::workspace::check_installed_rules([base, donor], [before, after], &policy, &overrides)?;

    let mut output: Option<Value> = None;
    // Each edit moves a retained edge to its unique baseline pin. That can
    // make a previously overlapping subtree portable on the next pass.
    loop {
        let current = output.as_ref().unwrap_or(donor);
        let plan = restorations(base, current, inputs.manifests, after, &policy, &overrides);
        if plan.edges.is_empty() {
            break;
        }
        let current = output.get_or_insert_with(|| donor.clone());
        plan.apply(base, current);
        graph::prune_scoped(current, DocumentKind::Project, donor, Some(after))?;
    }
    match output {
        Some(project) => Ok(baseline.converged_project(project, candidate)),
        None if baseline.environment() == candidate.environment() => Ok(candidate.clone()),
        None => Ok(baseline.converged_project(donor.clone(), candidate)),
    }
}

#[derive(Default)]
struct Restorations<'a> {
    // Only the baseline pin is borrowed. The candidate graph can be edited in place.
    edges: Vec<(&'static str, String, &'static str, String, &'a str)>,
    missing: BTreeSet<String>,
}

fn restorations<'a>(
    base: &'a Value,
    donor: &Value,
    changes: &[ManifestChange<'_>],
    after: &WorkspaceConfig,
    policy: &config::ResolutionCompatibility<'_>,
    overrides: &BTreeSet<&str>,
) -> Restorations<'a> {
    let peers = peer_names([base, donor], changes);
    let mut plan = Restorations::default();
    let mut blocked = None;
    let locks = [base, donor];
    for (section, groups) in [
        ("importers", schema::DEPENDENCY_GROUPS.as_slice()),
        (
            "snapshots",
            ["dependencies", "optionalDependencies"].as_slice(),
        ),
    ] {
        for (key, new) in graph::section(donor, section).into_iter().flatten() {
            let Some(old) = base[section].get(key).filter(|old| *old != new) else {
                continue;
            };
            let root = section == "importers";
            let release = pin::package_key(key);
            if !root
                && (!base["packages"][release]["resolution"]["integrity"].is_string()
                    || !assembly::same_package(
                        &base["packages"][release],
                        &donor["packages"][release],
                    )
                    || !policy.unchanged(release))
            {
                continue;
            }
            let mut providers = None;
            for &group in groups {
                for (name, entry) in graph::section(new, group).into_iter().flatten() {
                    let prev = &old[group][name];
                    let prior = if root { &prev["version"] } else { prev }.as_str();
                    let pin = if root { &entry["version"] } else { entry }.as_str();
                    let Some((prior, pin)) = prior.zip(pin).filter(|(a, b)| a != b) else {
                        continue;
                    };
                    if peers.contains(name.as_str())
                        || overrides.contains(name.as_str())
                        || root
                            && !entry["specifier"].as_str().is_some_and(|s| {
                                catalog::accepts_pin(name, s, prior, donor, Some(after))
                            })
                    {
                        continue;
                    }
                    let targets = [prior, pin].map(|v| format!("{name}@{v}"));
                    if !locks.into_iter().zip(&targets).all(|(doc, k)| {
                        let release = pin::package_key(k);
                        release.rsplit_once('@').is_some_and(|(n, v)| {
                            n == name && v.parse::<node_semver::Version>().is_ok()
                        }) && doc["packages"][release]["resolution"]["integrity"].is_string()
                            && doc["snapshots"].get(k).is_some()
                            && patch::hash_in_key(k).is_none()
                    }) || !policy.unchanged(&targets[0])
                    {
                        continue;
                    }
                    let old_key = &targets[0];
                    let pkg = pin::package_key(old_key);
                    if donor["packages"]
                        .get(pkg)
                        .is_some_and(|p| !assembly::same_package(p, &base["packages"][pkg]))
                    {
                        continue;
                    }
                    let ordinary = locks
                        .into_iter()
                        .zip(&targets)
                        .all(|(d, k)| assembly::ordinary(d, k));
                    if !ordinary {
                        if !root || pkg != pin::package_key(&targets[1]) {
                            continue;
                        }
                        // The prior consumer witnessed the complete old instance.
                        // Its external providers must still come from the same scope.
                        let scopes = providers.get_or_insert_with(|| {
                            locks.map(|d| {
                                let p = peer::importer_providers(
                                    &d["importers"],
                                    key,
                                    after.resolution.workspace_root_peers,
                                );
                                peer::canonical_providers(&p).into_owned()
                            })
                        });
                        let external = reconcile::peer_names(base, old_key);
                        if external != reconcile::peer_names(donor, &targets[1])
                            || external
                                .iter()
                                .any(|n| scopes[0].get(n) != scopes[1].get(n))
                        {
                            continue;
                        }
                    }
                    let absent = donor["snapshots"].get(old_key).is_none();
                    if absent || !ordinary {
                        if blocked
                            .get_or_insert_with(|| {
                                incompatible_subgraphs(base, donor, policy, overrides)
                            })
                            .contains(old_key)
                        {
                            continue;
                        }
                        if absent {
                            plan.missing.insert(old_key.clone());
                        }
                    } else if !assembly::ordinary(donor, old_key) {
                        continue;
                    }
                    plan.edges
                        .push((section, key.clone(), group, name.clone(), prior));
                }
            }
        }
    }
    plan
}

impl Restorations<'_> {
    fn apply(mut self, base: &Value, output: &mut Value) {
        for (section, key, group, name, pin) in self.edges {
            let dep = &mut output[section][&key][group][&name];
            if section == "importers" {
                dep["version"] = pin.into()
            } else {
                *dep = pin.into()
            }
        }
        while let Some(key) = self.missing.pop_first() {
            if output["snapshots"].get(&key).is_some() {
                continue;
            }
            let record = &base["snapshots"][&key];
            for (_, name, pin) in super::workspace::edges(record) {
                let target = graph::snapshot_key(
                    name,
                    pin.as_str().expect("validated edge"),
                    graph::section(base, "snapshots"),
                );
                if output["snapshots"].get(&target).is_none() {
                    self.missing.insert(target);
                }
            }
            let release = pin::package_key(&key);
            if output["packages"].get(release).is_none() {
                output["packages"][release] = base["packages"][release].clone();
            }
            output["snapshots"][&key] = record.clone();
        }
    }
}

fn check_manifests(
    [base, donor]: [&Value; 2],
    inputs: ChangeSet<'_>,
    before: &WorkspaceConfig,
    after: &WorkspaceConfig,
) -> Result<(), LockfileError> {
    let baseline_rules = manifest::Overrides::new(base, before);
    let candidate_rules = manifest::Overrides::new(donor, after);
    let mut supplied = HashSet::new();
    for change in inputs.manifests {
        if !supplied.insert(change.importer) {
            return Err(LockfileError::InvalidManifest(change.importer.into()));
        }
        super::check_baseline_manifest(&baseline_rules, change, Some(before), inputs)?;
        manifest::check(&candidate_rules, change.importer, change.after, "candidate")?;
    }
    // Importer pins can legitimately drift without a manifest change. A new
    // declaration or a different project needs the caller's manifest as evidence.
    for doc in [base, donor] {
        for id in graph::section(doc, "importers")
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
        {
            if supplied.contains(id.as_str()) {
                continue;
            }
            let [a, b] = [base, donor].map(|d| d["importers"].get(id));
            let unchanged = a.zip(b).is_some_and(|(a, b)| {
                a == b
                    || [a, b].into_iter().all(|r| {
                        r.as_object().unwrap().keys().all(|k| {
                            if schema::DEPENDENCY_GROUPS.contains(&k.as_str()) {
                                graph::section(r, k).into_iter().flatten().all(|(n, _)| {
                                    a[k][n].get("specifier") == b[k][n].get("specifier")
                                })
                            } else {
                                a.get(k) == b.get(k)
                            }
                        })
                    })
            });
            if !unchanged {
                return Err(manifest::mismatch(
                    id,
                    "candidate",
                    "include the manifest for this changed importer",
                ));
            }
        }
    }
    Ok(())
}

fn peer_names<'a>(docs: [&'a Value; 2], changes: &[ManifestChange<'a>]) -> HashSet<&'a str> {
    let mut peers = HashSet::new();
    for doc in docs {
        for (_, pkg) in graph::section(doc, "packages").into_iter().flatten() {
            peers.extend(
                graph::section(pkg, "peerDependencies")
                    .into_iter()
                    .flatten()
                    .map(|(n, _)| n.as_str()),
            );
        }
        for (_, body) in graph::section(doc, "snapshots").into_iter().flatten() {
            peers.extend(
                body["transitivePeerDependencies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
        }
    }
    for manifest in changes.iter().flat_map(|c| [c.before, c.after]).flatten() {
        peers.extend(
            graph::section(manifest, "peerDependencies")
                .into_iter()
                .flatten()
                .map(|(n, _)| n.as_str()),
        );
    }
    peers
}

/// A baseline-only subtree can be copied intact if every reachable artifact and
/// rule still agrees. Shared snapshot keys must describe the same dependency edges.
fn incompatible_subgraphs(
    base: &Value,
    donor: &Value,
    policy: &config::ResolutionCompatibility<'_>,
    changed: &BTreeSet<&str>,
) -> BTreeSet<String> {
    let removed: Vec<_> = graph::section(base, "overrides")
        .into_iter()
        .flatten()
        .filter_map(|(s, v)| {
            let (parent, child, _) = manifest::override_selector(s);
            (v == "-" && changed.contains(child)).then_some(parent)
        })
        .collect();
    super::affected_by(base, |key| {
        let release = pin::package_key(key);
        let body = &base["snapshots"][key];
        let package = &base["packages"][release];
        !package["resolution"]["integrity"].is_string()
            || !policy.unchanged(release)
            || patch::hash_in_key(key).is_some()
            || donor["packages"]
                .get(release)
                .is_some_and(|p| !assembly::same_package(p, package))
            || donor["snapshots"].get(key).is_some_and(|s| s != body)
            || super::workspace::edges(body).any(|(_, name, p)| {
                changed.contains(name.as_str())
                    || p.as_str().is_some_and(|p| p.starts_with("link:"))
            })
            || graph::section(package, "peerDependencies")
                .into_iter()
                .flatten()
                .any(|(n, _)| changed.contains(n.as_str()))
            || release.rsplit_once('@').is_some_and(|(n, v)| {
                removed
                    .iter()
                    .any(|parent| parent.is_none_or(|p| manifest::parent_matches(p, n, v)))
            })
    })
}
