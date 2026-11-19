//! Validate the configuration represented by each lock and select the final
//! catalog pins and importer edges required by the edited rules.
use std::collections::{BTreeMap, BTreeSet};

use crate::scope::importer_dependency;
use crate::{LockfileError, Value, WorkspaceConfig, graph, manifest, reconcile};
use reconcile::{metadata, missing};

#[derive(PartialEq, Eq)]
pub(in crate::scope) enum RootChoice {
    Baseline,
    Installed,
    /// A peer-free baseline release usable by a new catalog consumer.
    CatalogPin(Value),
}
pub(in crate::scope) type RootChoices = BTreeMap<String, BTreeMap<String, RootChoice>>;

fn catalog_name(spec: &str) -> Option<&str> {
    spec.strip_prefix("catalog:")
        .map(|s| if s.is_empty() { "default" } else { s })
}

pub(super) fn checked_overrides(
    doc: &Value,
    config: &WorkspaceConfig,
    root: Option<&Value>,
) -> Result<Option<Value>, LockfileError> {
    let expected = metadata::effective_overrides(config, root)?;
    let actual = doc.get("overrides");
    let mut header = serde_json::json!({});
    if let Some(ref value) = expected {
        header["overrides"] = value.clone();
    }
    let empty = |v: Option<&Value>| v.is_none_or(|v| v.as_object().is_some_and(|m| m.is_empty()));
    if !(manifest::same_overrides(doc, &header) || empty(actual) && empty(expected.as_ref())) {
        return Err(missing(
            &["overrides"],
            "locked selectors or their precedence differ from the workspace declarations",
        ));
    }
    Ok(if empty(expected.as_ref()) {
        actual.cloned()
    } else {
        expected
    })
}

pub(super) fn catalogs(
    output: &mut Value,
    docs: [&Value; 2],
    configs: [&WorkspaceConfig; 2],
    affected: &BTreeSet<String>,
) -> Result<RootChoices, LockfileError> {
    let mut choices = RootChoices::new();
    let mut consumers = BTreeMap::<_, Vec<_>>::new();
    for (id, root) in graph::section(docs[1], "importers").into_iter().flatten() {
        for group in manifest::GROUPS {
            for (name, dep) in graph::section(root, group).into_iter().flatten() {
                if let Some(catalog) = dep["specifier"].as_str().and_then(catalog_name) {
                    consumers
                        .entry((catalog, name.as_str()))
                        .or_default()
                        .push((id, group, dep));
                }
            }
        }
    }
    let [old, new] = configs.map(|c| &c.catalogs);
    let names: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    for catalog in names {
        let slots = [old.get(catalog), new.get(catalog)];
        let names: BTreeSet<_> = slots.iter().flatten().flat_map(|m| m.keys()).collect();
        for name in names {
            let [before, after] = slots.map(|m| m.and_then(|v| v.get(name)));
            let [base, installed] = docs.map(|d| d.get("catalogs")?.get(catalog)?.get(name));
            if before == after && base == installed {
                continue;
            }
            let error = || {
                missing(
                    &["catalogs", catalog, name],
                    "locked catalog entry does not match its workspace declaration",
                )
            };
            for (entry, spec) in [(base, before), (installed, after)] {
                if let Some(v) = entry {
                    let Some(spec) = spec else {
                        return Err(error());
                    };
                    if v["specifier"].as_str() != Some(spec)
                        || !v["version"].as_str().is_some_and(|pin| {
                            metadata::catalog_version_fits(spec, pin, Some(spec))
                        })
                    {
                        return Err(error());
                    }
                }
            }
            if let (Some(spec), Some(installed)) = (after, installed) {
                let mut entry = installed.clone();
                let users = consumers
                    .get(&(catalog.as_str(), name.as_str()))
                    .into_iter()
                    .flatten();
                for (id, group, dep) in users.clone() {
                    if !dep["version"].as_str().is_some_and(|pin| {
                        reconcile::accepts(
                            name,
                            dep["specifier"].as_str().expect("catalog reference"),
                            pin,
                            docs[1],
                        )
                    }) {
                        return Err(missing(
                            &["importers", id, group, name],
                            "installed pin differs from its catalog entry",
                        ));
                    }
                }
                // pnpm may omit the catalog record for an existing exact
                // release. A new consumer can use that same locked release.
                let (target, range) = reconcile::alias(spec).unwrap_or((name, spec));
                if before == after
                    && base.is_none()
                    && range.parse::<node_semver::Version>().is_ok()
                    && installed["version"].as_str() == Some(range)
                    && docs[0]["packages"]
                        .get(format!("{target}@{range}"))
                        .is_some()
                {
                    continue;
                }
                if let (Some(before), Some(pin)) =
                    (before, base.and_then(|v| v["version"].as_str()))
                    && reconcile::alias(before).map(|a| a.0) == reconcile::alias(spec).map(|a| a.0)
                    && metadata::catalog_version_fits(spec, pin, Some(before))
                {
                    let target = reconcile::alias(spec).map_or(name.as_str(), |(n, _)| n);
                    let key = format!("{target}@{pin}");
                    // A peer-free snapshot can serve a new importer directly. A
                    // contextual instance needs evidence from that consumer.
                    let portable = docs[0]["snapshots"].get(&key).is_some()
                        && !affected.contains(&key)
                        && reconcile::peer_names(docs[0], &key).is_empty();
                    let reference = reconcile::reference(name, &key);
                    let fits = |dep: &Value| {
                        dep["version"]
                            .as_str()
                            .is_some_and(|v| graph::package_key(v) == reference)
                    };
                    if users.clone().all(|(id, _, dep)| {
                        fits(dep)
                            || portable
                            || importer_dependency(docs[0], id, name).is_some_and(|(_, d)| fits(d))
                    }) {
                        entry["version"] = pin.into();
                        if portable {
                            for (id, _, dep) in users.clone() {
                                if !fits(dep)
                                    && !importer_dependency(docs[0], id, name)
                                        .is_some_and(|(_, d)| fits(d))
                                {
                                    let mut record = (*dep).clone();
                                    record["version"] = reference.as_str().into();
                                    choices
                                        .entry((*id).clone())
                                        .or_default()
                                        .insert(name.clone(), RootChoice::CatalogPin(record));
                                }
                            }
                        }
                    }
                }
                output["catalogs"][catalog][name] = entry;
            } else if let Some(m) = output
                .get_mut("catalogs")
                .and_then(|v| v.get_mut(catalog))
                .and_then(Value::as_object_mut)
            {
                m.remove(name);
                if m.is_empty() {
                    output["catalogs"].as_object_mut().unwrap().remove(catalog);
                }
            }
        }
    }
    if output
        .get("catalogs")
        .and_then(Value::as_object)
        .is_some_and(|m| m.is_empty())
        && docs[1].get("catalogs").is_none()
    {
        output.as_object_mut().unwrap().remove("catalogs");
    }
    Ok(choices)
}

pub(in crate::scope) fn override_names(docs: [&Value; 2]) -> BTreeSet<&str> {
    let groups = docs.map(|d| {
        let mut by_name = BTreeMap::<_, Vec<_>>::new();
        for (selector, value) in graph::section(d, "overrides").into_iter().flatten() {
            by_name
                .entry(manifest::override_selector(selector).1)
                .or_default()
                .push((selector, value));
        }
        by_name
    });
    groups[0]
        .keys()
        .chain(groups[1].keys())
        .copied()
        .filter(|n| groups[0].get(n) != groups[1].get(n))
        .collect()
}

pub(super) fn importer_choices(
    docs: [&Value; 2],
    configs: [&WorkspaceConfig; 2],
    output: &Value,
    overrides: &BTreeSet<&str>,
    mut choices: RootChoices,
) -> Result<RootChoices, LockfileError> {
    if configs[0].catalogs == configs[1].catalogs
        && overrides.is_empty()
        && docs[0].get("catalogs") == docs[1].get("catalogs")
    {
        return Ok(choices);
    }
    for (id, root) in graph::section(docs[0], "importers").into_iter().flatten() {
        let Some(current) = docs[1]["importers"].get(id) else {
            continue;
        };
        let names: BTreeSet<_> = manifest::GROUPS
            .into_iter()
            .flat_map(|g| {
                [root, current]
                    .into_iter()
                    .filter_map(move |r| graph::section(r, g))
            })
            .flat_map(|deps| deps.keys())
            .collect();
        for name in names {
            if choices.get(id).and_then(|m| m.get(name)).is_some() {
                continue;
            }
            // A manifest can move a name between dependency groups.
            let found = docs.map(|doc| importer_dependency(doc, id, name));
            let entries = found.map(|d| d.map(|(_, v)| v));
            let [old, new] = entries.map(|e| e.and_then(|v| v["specifier"].as_str()));
            let changed_catalog = [old, new]
                .into_iter()
                .flatten()
                .filter_map(catalog_name)
                .any(|c| {
                    configs[0].catalogs.get(c).and_then(|m| m.get(name))
                        != configs[1].catalogs.get(c).and_then(|m| m.get(name))
                        || docs[0]["catalogs"][c].get(name) != docs[1]["catalogs"][c].get(name)
                });
            if !(changed_catalog || overrides.contains(name.as_str()) && old != new) {
                continue;
            }
            let choice = match (entries[0], entries[1], new) {
                (Some(left), Some(right), Some(spec)) => {
                    let catalog = catalog_name(spec);
                    let entry = catalog.and_then(|c| output["catalogs"][c].get(name));
                    let effective = if let Some(c) = catalog {
                        configs[1]
                            .catalogs
                            .get(c)
                            .and_then(|m| m.get(name))
                            .ok_or_else(|| {
                                missing(
                                    &["catalogs", c, name],
                                    "referenced catalog declaration is missing",
                                )
                            })?
                            .as_str()
                    } else {
                        spec
                    };
                    let accepts = |dep: &Value, doc: &Value| {
                        dep["version"].as_str().is_some_and(|pin| {
                            reconcile::accepts(name, effective, pin, doc)
                                || (effective.parse::<node_semver::Range>().is_err()
                                    && reconcile::alias(effective).is_none()
                                    && dep == right)
                        })
                    };
                    if !accepts(right, docs[1]) {
                        return Err(missing(
                            &["importers", id, found[1].expect("installed edge").0, name],
                            "installed pin does not satisfy the final declaration",
                        ));
                    }
                    let fits_catalog = entry.is_none()
                        || left["version"]
                            .as_str()
                            .is_some_and(|p| reconcile::accepts(name, spec, p, output));
                    if accepts(left, docs[0]) && fits_catalog {
                        RootChoice::Baseline
                    } else {
                        RootChoice::Installed
                    }
                }
                _ => RootChoice::Installed,
            };
            choices
                .entry(id.clone())
                .or_default()
                .insert(name.clone(), choice);
        }
    }
    Ok(choices)
}

/// Check the installed records used as a unit. A package extension fills
/// missing published fields. An unconditional override can replace that value.
pub(in crate::scope) fn check_installed_rules(
    [base, donor]: [&Value; 2],
    [before, after]: [&WorkspaceConfig; 2],
    policy: &crate::config::ResolutionCompatibility<'_>,
    changed: &BTreeSet<&str>,
) -> Result<(), LockfileError> {
    let previous = manifest::Overrides::new(base, before.override_tie_break);
    let current = manifest::Overrides::new(donor, after.override_tie_break);
    let mut samples = std::collections::HashMap::new();
    for (key, body) in graph::section(base, "snapshots").into_iter().flatten() {
        samples.entry(graph::package_key(key)).or_insert(body);
    }
    for (key, body) in graph::section(donor, "snapshots").into_iter().flatten() {
        let pkg = graph::package_key(key);
        let Some((parent, version)) = pkg.rsplit_once('@') else {
            continue;
        };
        let old = base["snapshots"]
            .get(key)
            .or_else(|| samples.get(pkg).copied());
        let new_pkg = &donor["packages"][pkg];
        let expected = if !policy.unchanged(pkg) {
            policy.effective_extensions(pkg).ok_or_else(|| {
                missing(&["packageExtensions", pkg], "invalid extension declaration")
            })?
        } else {
            Default::default()
        };
        for (index, field) in ["peerDependencies", "peerDependenciesMeta"]
            .into_iter()
            .enumerate()
        {
            for (&name, &val) in &expected[1][index + 2] {
                if expected[0][index + 2].get(name) != Some(&val)
                    && base["packages"][pkg][field].get(name).is_none()
                    && new_pkg[field].get(name) != Some(val)
                    && current.relevant(name, parent, version).is_empty()
                {
                    return Err(missing(
                        &["packages", pkg, field, name],
                        "installed package is missing the declared peer extension",
                    ));
                }
            }
        }
        let changed_edges = super::edges(body)
            .map(|(_, n, _)| n.as_str())
            .chain(
                old.into_iter()
                    .flat_map(super::edges)
                    .map(|(_, n, _)| n.as_str()),
            )
            .filter(|n| changed.contains(*n))
            .chain(expected[1][0].keys().chain(expected[1][1].keys()).copied())
            .collect::<BTreeSet<_>>();
        for name in changed_edges {
            if new_pkg["peerDependencies"].get(name).is_some() {
                continue;
            }
            let required = expected[1][0].get(name);
            let ext = expected[1][1]
                .get(name)
                .or(required)
                .and_then(|v| v.as_str());
            let rules = current.relevant(name, parent, version);
            let overridden = changed.contains(name)
                && previous.relevant(name, parent, version) != rules
                && !rules.is_empty()
                && rules
                    .iter()
                    .all(|(s, _)| manifest::override_selector(s).2.is_none());
            let Some(spec) = ext.or(overridden.then_some("*")) else {
                continue;
            };
            let parent = serde_json::json!({"name":parent,"version":version});
            let spec = current.apply(&parent, ".", name, spec)?;
            let pin = super::dependency(body, name);
            let previous_edge = old.and_then(|r| super::dependency(r, name));
            let valid = match (spec.as_deref(), pin) {
                (None, p) => p.is_none(),
                (Some(_), None) => required.is_none() || expected[1][1].contains_key(name),
                (Some(s), Some((_, p))) if overridden || previous_edge.is_none() => {
                    s.parse::<node_semver::Range>().is_err() && reconcile::alias(s).is_none()
                        || reconcile::accepts(name, s, p, donor)
                }
                _ => true,
            };
            if !valid {
                return Err(missing(
                    &["snapshots", key, name],
                    "installed dependency does not match the final workspace rule",
                ));
            }
        }
    }
    Ok(())
}
