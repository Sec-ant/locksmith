//! Keep every compatible importer pin and update only stale declarations.
use super::Evidence;
use crate::{
    LockfileError, Value, catalog, error::missing, graph, manifest, pin, reconcile, schema,
};
use serde_json::{Map, json};
use std::{borrow::Cow, collections::BTreeSet};

pub(super) fn repair<'a>(
    doc: &'a Value,
    metadata: &Value,
    evidence: &mut Evidence<'_>,
) -> Result<Cow<'a, Value>, LockfileError> {
    let context = evidence.context;
    let mut result = Cow::Borrowed(doc);
    let mut used_catalogs = BTreeSet::new();
    let links =
        crate::workspace_links::WorkspaceLinks::new(context.manifests, context.configuration);
    let overrides = manifest::Overrides::new(metadata, context.configuration);
    let allow_importer_edit = |id: &str| {
        if context.configuration.resolution.pnpmfile_checksum.is_some() {
            return Err(missing(
                &["importers", id],
                "pnpmfile may transform this manifest; repairing its importer requires the effective declarations",
            ));
        }
        Ok(())
    };
    for id in graph::section(doc, "importers")
        .into_iter()
        .flat_map(|m| m.keys())
    {
        if !context.manifests.contains_key(id) {
            allow_importer_edit(id)?;
            result.to_mut()["importers"]
                .as_object_mut()
                .expect("importers")
                .remove(id);
        }
    }
    for (id, manifest) in context.manifests {
        if !manifest.is_object() {
            return Err(LockfileError::InvalidManifest(id.clone()));
        }
        let record = doc["importers"].get(id);
        let empty = json!({});
        let old = record.unwrap_or(&empty);
        if !old.is_object() {
            return Err(missing(&["importers", id], "expected an importer mapping"));
        }
        let desired = manifest::declarations(manifest, &overrides, id)?;
        let mut next = Cow::Borrowed(old);
        for group in schema::DEPENDENCY_GROUPS {
            for key in graph::section(old, group)
                .into_iter()
                .flat_map(|m| m.keys())
            {
                if desired.get(key.as_str()).is_none_or(|(g, _)| *g != group) {
                    let map = next.to_mut()[group]
                        .as_object_mut()
                        .expect("dependency group");
                    map.remove(key);
                    if map.is_empty() {
                        next.to_mut().as_object_mut().unwrap().remove(group);
                    }
                }
            }
            if old.get(group).is_some_and(|g| !g.is_object()) {
                return Err(missing(
                    &["importers", id, group],
                    "expected a dependency mapping",
                ));
            }
        }
        for (name, (group, spec)) in desired {
            let catalog = spec
                .strip_prefix("catalog:")
                .map(|c| if c.is_empty() { "default" } else { c });
            let range = if let Some(catalog) = catalog {
                let range = context
                    .configuration
                    .catalogs
                    .get(catalog)
                    .and_then(|c| c.get(name))
                    .ok_or_else(|| {
                        missing(
                            &["catalogs", catalog, name],
                            "final workspace has no declaration",
                        )
                    })?;
                range.as_str()
            } else {
                &spec
            };
            let prior = old[group].get(name).or_else(|| {
                schema::DEPENDENCY_GROUPS
                    .into_iter()
                    .find_map(|g| old[g].get(name))
            });
            let prior_link = prior
                .and_then(|dep| dep["version"].as_str())
                .is_some_and(|pin| pin.starts_with("link:"));
            let workspace_link = if prior_link {
                links.resolve(name, &spec, id, manifest)?
            } else {
                None
            };
            let retained = prior.and_then(|dep| dep["version"].as_str()).filter(|v| {
                if let Some((link, _)) = &workspace_link {
                    return link == v;
                }
                if (spec.starts_with("workspace:") || catalog.is_some()) && v.starts_with("link:") {
                    return false;
                }
                fits(
                    name,
                    range,
                    v,
                    prior.and_then(|d| d["specifier"].as_str()) == Some(spec.as_ref()),
                    doc,
                    context.configuration,
                )
            });
            if let Some(catalog) = catalog {
                used_catalogs.insert((catalog.to_owned(), name.to_owned()));
                // pnpm accepts a satisfied importer even when the lock contains
                // no snapshot for this catalog entry.
                let current = result["catalogs"][catalog].get(name);
                let entry_fits = |entry: &Value| {
                    if workspace_link.is_some() {
                        catalog::link_entry_fits(range, entry)
                    } else {
                        catalog_fits(range, entry)
                    }
                };
                if retained.is_none() || current.is_some_and(|entry| !entry_fits(entry)) {
                    let proposed = metadata["catalogs"][catalog].get(name);
                    let entry = if let Some(entry) = proposed.filter(|e| catalog_fits(range, e)) {
                        entry.clone()
                    } else if let Some(pin) = retained {
                        let base = pin::package_key(pin);
                        let effective =
                            pin::pinned_specifier(range, pin, Some(context.configuration));
                        let target = pin::alias(effective).map_or(name, |(target, _)| target);
                        let version = pin::version_from_pin(name, target, base).unwrap_or(base);
                        json!({"specifier":range,"version":version})
                    } else {
                        return Err(reconcile::published::need_version(
                            &["catalogs", catalog, name],
                            name,
                            range,
                            context.configuration,
                            "missing catalog resolution".into(),
                        ));
                    };
                    if current != Some(&entry) {
                        result.to_mut()["catalogs"][catalog][name] = entry;
                    }
                }
            }
            let dangling = retained.filter(|pin| pin.contains('(')).is_some_and(|pin| {
                let key = graph::snapshot_key(name, pin, graph::section(doc, "snapshots"));
                doc["packages"].get(pin::package_key(&key)).is_some()
                    && doc["snapshots"].get(&key).is_none()
                    && context.parents.iter().all(|(lock, _)| {
                        lock.project()
                            .and_then(|p| p["snapshots"].get(&key))
                            .is_none()
                    })
            });
            let version = match retained {
                Some(v) if !dangling => v,
                prior => {
                    let candidate = evidence.resolved()?["importers"][id][group][name]["version"]
                        .as_str()
                        .ok_or_else(|| {
                            missing(
                                &["importers", id, group, name],
                                "no resolved dependency for the final declaration",
                            )
                        })?;
                    if prior.is_some_and(|v| pin::package_key(v) != pin::package_key(candidate)) {
                        return Err(missing(
                            &["importers", id, group, name],
                            "no peer instance for the retained release",
                        ));
                    }
                    candidate
                }
            };
            if old[group][name]["specifier"].as_str() != Some(&spec)
                || old[group][name]["version"].as_str() != Some(version)
            {
                let mut dep = prior.cloned().unwrap_or_else(|| json!({}));
                if !dep.is_object() {
                    return Err(missing(
                        &["importers", id, group, name],
                        "expected a dependency mapping",
                    ));
                }
                dep["specifier"] = json!(spec);
                dep["version"] = json!(version);
                next.to_mut()[group][name] = dep;
            }
        }
        for (key, value) in [
            (
                "dependenciesMeta",
                manifest
                    .get("dependenciesMeta")
                    .filter(|m| !m.as_object().is_some_and(Map::is_empty)),
            ),
            (
                "linkDirectory",
                manifest::link_directory(manifest, context.configuration),
            ),
            (
                "publishDirectory",
                manifest["publishConfig"].get("directory"),
            ),
        ] {
            if old.get(key) != value {
                match value {
                    Some(v) => next.to_mut()[key] = v.clone(),
                    None => {
                        next.to_mut().as_object_mut().unwrap().remove(key);
                    }
                }
            }
        }
        if record.is_none() || matches!(next, Cow::Owned(_)) {
            allow_importer_edit(id)?;
            result.to_mut()["importers"][id] = next.into_owned();
        }
    }
    // pnpm compares every catalog snapshot in the lock with the workspace,
    // including entries left behind by a removed importer.
    for (catalog, entries) in graph::section(doc, "catalogs").into_iter().flatten() {
        for (name, entry) in entries
            .as_object()
            .ok_or_else(|| missing(&["catalogs", catalog], "expected a catalog mapping"))?
        {
            if !used_catalogs.contains(&(catalog.clone(), name.clone()))
                && context
                    .configuration
                    .catalogs
                    .get(catalog)
                    .and_then(|m| m.get(name))
                    .is_none_or(|r| !catalog_fits(r, entry))
            {
                let catalogs = result.to_mut()["catalogs"]
                    .as_object_mut()
                    .expect("catalogs");
                let group = catalogs[catalog].as_object_mut().expect("catalog group");
                group.remove(name);
                if group.is_empty() {
                    catalogs.remove(catalog);
                }
                if catalogs.is_empty() {
                    result.to_mut().as_object_mut().unwrap().remove("catalogs");
                }
            }
        }
    }
    Ok(result)
}

fn catalog_fits(spec: &str, entry: &Value) -> bool {
    catalog::entry_version(spec, entry).is_some()
}

fn fits(
    name: &str,
    range: &str,
    version: &str,
    same_spec: bool,
    doc: &Value,
    config: &crate::WorkspaceConfig,
) -> bool {
    if version.starts_with("link:") {
        return same_spec;
    }
    let range = pin::pinned_specifier(range, version, Some(config));
    if range == "*"
        && pin::version_from_pin(name, name, pin::package_key(version))
            .is_some_and(|release| release.parse::<node_semver::Version>().is_ok())
    {
        return true;
    }
    if range.starts_with("npm:") || range.parse::<node_semver::Range>().is_ok() {
        return catalog::accepts_pin(name, range, version, doc, Some(config));
    }
    same_spec
}
