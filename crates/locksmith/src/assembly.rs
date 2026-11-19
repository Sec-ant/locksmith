use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::{Conflict, ConflictKind, DocumentKind, Lockfile, LockfileError, Value, graph};

/// Assemble a graph from independently selected importer edges. The large
/// input graphs stay borrowed until their selected records are copied once.
pub(crate) fn from_edges(
    (mut base, donor): (Lockfile, &Lockfile),
    original: &Value,
    importers: Value,
    [retained, adopted]: [&Value; 2],
    options: crate::ConvergeOptions<'_>,
    affected: [&BTreeSet<String>; 2],
    generated: BTreeMap<String, Value>,
) -> Result<Lockfile, LockfileError> {
    let baseline = base.project().expect("converge checked project document");
    let current = donor.project().expect("converge checked project document");
    let selected = graph::closure_from_importers(current, adopted)?;
    let (mut edits, restored) =
        retain_children(baseline, current, &selected.snapshots, options, affected)?;
    let protected = graph::closure_from_edges(baseline, retained, &restored)?;
    let mut conflicts = Vec::new();
    for key in selected.snapshots.intersection(&protected.snapshots) {
        let previous = &baseline["snapshots"][key];
        // Optionality describes the paths to this instance. Compare the
        // dependency records first and derive the flag from the final graph.
        let installed = &current["snapshots"][key];
        if previous.get("optional") != installed.get("optional") {
            let record = edits
                .entry(key.clone())
                .or_insert_with(|| installed.clone());
            copy_optional(record, previous);
        }
        let chosen = edits.get(key).unwrap_or(installed);
        let release = graph::package_key(key);
        // Both paths selected the same immutable parent instance. Its earlier
        // child peer bindings remain usable when all releases are the same and
        // the manifest edits cannot reach a provider in either subgraph.
        if previous != chosen
            && !affected.iter().any(|keys| keys.contains(key))
            && baseline["packages"][release]["resolution"]["integrity"].is_string()
            && baseline["packages"][release] == current["packages"][release]
            && crate::peer::same_releases(previous, chosen)
        {
            edits.insert(key.clone(), previous.clone());
            continue;
        }
        protect(baseline, Some(chosen), "snapshots", key, &mut conflicts);
    }
    for key in selected.packages.intersection(&protected.packages) {
        if same_package(&baseline["packages"][key], &current["packages"][key]) {
            continue;
        }
        protect(
            baseline,
            current["packages"].get(key),
            "packages",
            key,
            &mut conflicts,
        );
    }
    let docs = std::mem::take(&mut base.documents);
    let mut result = Lockfile::with_documents(docs, &[&base, donor]);
    result.preserve_baseline = true;
    // Projection already owns this graph. Only a borrowed input needs a copy.
    let output = std::sync::Arc::make_mut(
        result
            .documents
            .get_mut(&DocumentKind::Project)
            .expect("project checked"),
    );
    output["importers"] = importers;
    for key in &selected.snapshots {
        replace_entry(output, current, "snapshots", key);
    }
    for key in &selected.packages {
        if !same_package(&output["packages"][key], &current["packages"][key]) {
            replace_entry(output, current, "packages", key);
        }
    }
    for (key, body) in edits {
        output["snapshots"][key] = body;
    }
    for (key, mut body) in generated {
        if protected.snapshots.contains(&key) || selected.snapshots.contains(&key) {
            copy_optional(&mut body, &output["snapshots"][&key]);
            protect(output, Some(&body), "snapshots", &key, &mut conflicts);
        } else {
            output["snapshots"][key] = body;
        }
    }
    complete(
        output,
        DocumentKind::Project,
        Some((original, options.configuration)),
        &mut conflicts,
    )?;
    if !conflicts.is_empty() {
        return Err(LockfileError::Conflicts(conflicts));
    }
    Ok(result)
}

/// Shared completion policy for convergence and merges.
/// Only reachable graph conflicts survive pruning; other conflicts remain atomic.
pub(crate) fn complete(
    document: &mut Value,
    kind: DocumentKind,
    baseline: Option<(&Value, Option<&crate::WorkspaceConfig>)>,
    conflicts: &mut Vec<Conflict>,
) -> Result<(), LockfileError> {
    if conflicts.iter().all(|conflict| {
        conflict.path.len() == 2 && ["packages", "snapshots"].contains(&conflict.path[0].as_str())
    }) {
        if let Some((baseline, config)) = baseline {
            graph::prune_scoped(document, kind, baseline, config)?;
        } else {
            graph::prune(document, kind)?;
        }
        conflicts.retain(|conflict| {
            document
                .get(&conflict.path[0])
                .and_then(|map| map.get(&conflict.path[1]))
                .is_some()
        });
    }
    Ok(())
}

// A registry deprecation notice may change after the immutable artifact was published.
pub(crate) fn same_package(a: &Value, b: &Value) -> bool {
    a == b
        || a["resolution"]["integrity"].is_string()
            && a.get("deprecated") != b.get("deprecated")
            && [a, b].into_iter().all(|doc| {
                doc.as_object()
                    .is_some_and(|m| m.keys().all(|k| k == "deprecated" || a.get(k) == b.get(k)))
            })
}

fn copy_optional(record: &mut Value, source: &Value) {
    if let Some(value) = source.get("optional") {
        record["optional"] = value.clone();
    } else {
        record.as_object_mut().expect("snapshot").remove("optional");
    }
}

fn protect(
    base: &Value,
    new: Option<&Value>,
    section: &str,
    key: &str,
    conflicts: &mut Vec<Conflict>,
) {
    let old = base.get(section).and_then(|map| map.get(key));
    if old != new {
        conflicts.push(Conflict {
            document: DocumentKind::Project,
            path: vec![section.into(), key.into()],
            kind: ConflictKind::SharedDependency,
            base: old.cloned(),
            ours: old.cloned(),
            theirs: new.cloned(),
        });
    }
}

fn replace_entry(output: &mut Value, donor: &Value, section: &str, key: &str) {
    let new = donor.get(section).and_then(|map| map.get(key));
    let root = output
        .as_object_mut()
        .expect("parsed document is a mapping");
    if let Some(value) = new {
        root.entry(section)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("parsed section is a mapping")
            .insert(key.into(), value.clone());
    } else if let Some(map) = root.get_mut(section).and_then(Value::as_object_mut) {
        map.remove(key);
    }
}

/// Prefer a previously locked child on an adopted edge. A baseline instance of
/// the same parent witnesses the old edge. For a new parent release its original
/// declaration is needed before replacing pnpm's pin.
fn retain_children(
    base: &Value,
    candidate: &Value,
    selected: &BTreeSet<String>,
    options: crate::ConvergeOptions<'_>,
    affected: [&BTreeSet<String>; 2],
) -> Result<(BTreeMap<String, Value>, BTreeSet<String>), LockfileError> {
    let mut edits = BTreeMap::new();
    let mut restored = BTreeSet::new();
    let default_config = crate::WorkspaceConfig::default();
    let config = options.configuration.unwrap_or(&default_config);
    let overrides = crate::manifest::Overrides::new(base, config.override_tie_break);
    let mut versions = None;
    let lookup = options.registry.is_some()
        && base.get("pnpmfileChecksum").is_none()
        && (base.get("packageExtensionsChecksum").is_none()
            || config.resolution.package_extensions.is_some());
    for key in selected {
        let parent = graph::package_key(key);
        let current = &candidate["snapshots"][key];
        let old = &base["snapshots"][key];
        if old == current || (!old.is_object() && !lookup) || affected[1].contains(key) {
            continue;
        }
        let Some((parent_name, version)) = parent.rsplit_once('@') else {
            continue;
        };
        let pkg = &candidate["packages"][parent];
        let witnessed = !old.is_null() && same_package(pkg, &base["packages"][parent]);
        if !pkg["resolution"]["integrity"].is_string()
            || (!witnessed && crate::peer::patch(key).is_some())
        {
            continue;
        }
        let parent_manifest = json!({"name":parent_name,"version":version});
        for group in ["dependencies", "optionalDependencies"] {
            for (name, value) in graph::section(current, group).into_iter().flatten() {
                let Some(pin) = value.as_str() else { continue };
                if old[group].get(name) == Some(value)
                    || pkg["peerDependencies"].get(name).is_some()
                {
                    continue;
                }
                let child = graph::snapshot_key(name, pin, graph::section(candidate, "snapshots"));
                let Some((actual, v)) = graph::package_key(&child).rsplit_once('@') else {
                    continue;
                };
                if v.parse::<node_semver::Version>().is_err() || affected[1].contains(&child) {
                    continue;
                }
                let previous = old[group][name]
                    .as_str()
                    .map(|r| graph::snapshot_key(name, r, graph::section(base, "snapshots")));
                let reusable = |k: &str| {
                    ordinary(base, k)
                        && !affected[0].contains(k)
                        && k.rsplit_once('@').is_some_and(|(n, _)| n == actual)
                };
                let pick = if witnessed {
                    previous
                        .as_deref()
                        .filter(|&k| {
                            !affected[0].contains(k)
                                && same_child_context(base, k, candidate, &child, actual)
                        })
                        .map(str::to_owned)
                } else if lookup
                    && ordinary(candidate, &child)
                    && base["snapshots"].get(&child).is_none()
                {
                    let index = versions.get_or_insert_with(|| {
                        let mut map = HashMap::<_, Vec<_>>::new();
                        for k in graph::section(base, "snapshots")
                            .into_iter()
                            .flat_map(|m| m.keys())
                        {
                            if let Some((name, v)) = k.rsplit_once('@')
                                && !k.contains('(')
                                && let Ok(v) = v.parse::<node_semver::Version>()
                            {
                                map.entry(name).or_default().push((v, k.as_str()));
                            }
                        }
                        map
                    });
                    if options
                        .registry
                        .is_some_and(|data| data.unavailable_manifests.contains(parent))
                    {
                        continue;
                    }
                    let Some(choices) = index.get(actual) else {
                        continue;
                    };
                    let Some(decl) = crate::reconcile::published::declaration(
                        options.registry,
                        config,
                        pkg,
                        parent,
                        group,
                        name,
                        "original declaration is needed to retain an existing transitive pin",
                    )?
                    else {
                        continue;
                    };
                    let Some(range) = overrides.apply(&parent_manifest, ".", name, &decl)? else {
                        continue;
                    };
                    let valid = |k: &str| {
                        reusable(k)
                            && crate::reconcile::accepts(
                                name,
                                &range,
                                super::reconcile::reference(name, k).as_str(),
                                base,
                            )
                    };
                    previous
                        .as_deref()
                        .filter(|k| valid(k))
                        .or_else(|| {
                            choices
                                .iter()
                                .filter(|(_, k)| valid(k))
                                .max_by(|(l, _), (r, _)| l.cmp(r))
                                .map(|(_, k)| *k)
                        })
                        .map(str::to_owned)
                } else {
                    None
                };
                if let Some(pick) = pick.filter(|k| k != &child) {
                    edits.entry(key.clone()).or_insert_with(|| current.clone())[group][name] =
                        Value::from(crate::reconcile::reference(name, &pick));
                    restored.insert(pick);
                }
            }
        }
    }
    Ok((edits, restored))
}

fn same_child_context(base: &Value, old: &str, installed: &Value, new: &str, name: &str) -> bool {
    let previous = graph::package_key(old);
    let current = graph::package_key(new);
    let Some((old_name, version)) = previous.rsplit_once('@') else {
        return false;
    };
    old_name == name
        && version.parse::<node_semver::Version>().is_ok()
        && old[previous.len()..] == new[current.len()..]
        && base["packages"][previous]["resolution"]["integrity"].is_string()
        && installed["packages"][current]["resolution"]["integrity"].is_string()
}

pub(crate) fn ordinary(doc: &Value, key: &str) -> bool {
    !key.contains('(')
        && doc["snapshots"].get(key).is_some()
        && doc["packages"][key]["resolution"]["integrity"].is_string()
        && doc["packages"][key].get("peerDependencies").is_none()
        && doc["snapshots"][key]
            .get("transitivePeerDependencies")
            .is_none()
}
