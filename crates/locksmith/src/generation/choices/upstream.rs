use std::collections::{BTreeMap, BTreeSet};

use super::{Choices, Demand, Demands, Edge};
use crate::generation::{
    RegistryPackage, Result,
    graph::{Draft, Sources},
};
use crate::{Value, WorkspaceConfig, graph, manifest};

/// Reconsider only new selections on paths to releases introduced by this edit.
pub(in crate::generation) fn reuse_children(
    doc: &Value,
    old: &Value,
    draft: &Draft,
    demands: &Demands,
    cache: &mut Sources<'_>,
    config: &WorkspaceConfig,
    pins: &Choices,
) -> Result<Vec<Choices>> {
    let mut live = BTreeMap::<RegistryPackage, BTreeSet<&str>>::new();
    for (key, _) in graph::section(doc, "packages").into_iter().flatten() {
        if let Some((package, version)) = RegistryPackage::from_key(key) {
            live.entry(package).or_default().insert(version);
        }
    }
    let duplicated = super::introduced_duplicates(doc, old);
    let affected = super::ancestors(draft, &duplicated);
    let mut parents = BTreeMap::<&str, BTreeMap<&str, BTreeSet<&str>>>::new();
    for node in draft.nodes.values() {
        for (alias, edge) in &node.edges {
            let child = graph::package_key(&edge.target);
            if !duplicated.contains(child) {
                continue;
            }
            let Some((package, version)) = RegistryPackage::from_key(child) else {
                continue;
            };
            if package.name != *alias || package.prefix.is_some() {
                continue;
            }
            if let Some(others) = live.get(&package).filter(|vs| vs.len() > 1) {
                parents
                    .entry(&node.package)
                    .or_default()
                    .entry(alias)
                    .or_default()
                    .extend(others.iter().copied().filter(|v| *v != version));
            }
        }
    }
    let mut families = BTreeMap::<String, Vec<(&Edge, &Demand)>>::new();
    for (edge, demand) in demands {
        let key = demand.package.key(&demand.selected);
        if demand.selector.parse::<node_semver::Version>().is_ok() {
            continue;
        }
        let search = !demand.incumbent && !pins.contains_key(edge) && affected.contains(&key);
        if search {
            families.entry(key).or_default().push((edge, demand));
        }
    }
    for (key, _) in graph::section(old, "packages").into_iter().flatten() {
        if let Some((package, version)) = RegistryPackage::from_key(key) {
            live.entry(package).or_default().insert(version);
        }
    }
    let overrides = manifest::Overrides::new(doc, config);
    let mut moves = Vec::new();
    for (current, edges) in families {
        let root = edges
            .iter()
            .any(|(e, _)| matches!(e.0, super::Owner::Importer(_)));
        let children = parents.get(current.as_str());
        let reusable = |raw: &Value| -> Result<Vec<bool>> {
            children
                .into_iter()
                .flatten()
                .map(|(&name, other)| {
                    let Some(spec) = raw["optionalDependencies"]
                        .get(name)
                        .or_else(|| raw["dependencies"].get(name))
                    else {
                        return Ok(true);
                    };
                    let Some(spec) = spec.as_str() else {
                        return Ok(false);
                    };
                    let replacement = overrides.select(raw, name, spec)?;
                    let range = match replacement {
                        Some(value) => overrides.resolve(".", name, value)?,
                        None => Some(std::borrow::Cow::Borrowed(spec)),
                    };
                    let Some(range) = range else { return Ok(true) };
                    Ok(other
                        .iter()
                        .any(|v| manifest::matches_version(&range, Some(v))))
                })
                .collect()
        };
        let Some(current_release) = cache.speculative_release(&current)? else {
            continue;
        };
        let before = reusable(&current_release.manifest)?;
        if !root && before.iter().all(|reused| *reused) {
            continue;
        }
        // A root selects a whole subtree. Two releases may force different
        // grandchildren even when their direct children have no duplicate.
        // Group releases by the direct constraints observable on this graph.
        // A dependency whose current subtree reaches no introduced duplicate
        // cannot remove one, so its constraint does not distinguish releases.
        let mut present = BTreeSet::new();
        let mut related = BTreeSet::new();
        for node in draft.nodes.values().filter(|node| node.package == current) {
            for (name, edge) in &node.edges {
                present.insert(name.as_str());
                if affected.contains(graph::package_key(&edge.target)) {
                    related.insert(name.as_str());
                }
            }
        }
        let shape = |raw: &Value| -> Result<Vec<_>> {
            let mut shape = Vec::new();
            for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
                for (name, value) in graph::section(raw, group).into_iter().flatten() {
                    if present.contains(name.as_str()) && !related.contains(name.as_str()) {
                        continue;
                    }
                    let Some(spec) = value.as_str() else { continue };
                    let range = match overrides.select(raw, name, spec)? {
                        Some(value) => overrides.resolve(".", name, value)?,
                        None => Some(std::borrow::Cow::Borrowed(spec)),
                    };
                    let Some(range) = range else { continue };
                    let target = RegistryPackage::selector(name, &range);
                    let observed = target.as_ref().and_then(|(pkg, selector)| {
                        Some(
                            live.get(pkg)?
                                .iter()
                                .filter(|v| manifest::matches_version(selector, Some(v)))
                                .map(|v| (*v).to_owned())
                                .collect::<Vec<_>>(),
                        )
                    });
                    let optional_peer = group == "peerDependencies"
                        && raw["peerDependenciesMeta"][name]["optional"] == true;
                    shape.push((
                        group,
                        name.clone(),
                        // An alias can name a different package with the same versions.
                        target.map(|(pkg, _)| pkg.name),
                        // Without a locked witness, different selectors can lead
                        // to different grandchildren, even for the same child name.
                        observed
                            .filter(|vs| !vs.is_empty())
                            .unwrap_or_else(|| vec![range.into_owned()]),
                        optional_peer,
                    ));
                }
            }
            Ok(shape)
        };
        let mut seen = BTreeSet::new();
        let mut profiles = BTreeSet::new();
        let mut shapes = BTreeSet::new();
        if root {
            shapes.insert(shape(&current_release.manifest)?);
        }
        let domains: Vec<_> = edges
            .iter()
            .map(|(edge, d)| {
                if matches!(edge.0, super::Owner::Importer(_)) {
                    // A reusable release elsewhere in the old lock is a feasible
                    // initial choice. The changed declaration can admit other releases.
                    if let Ok(eligible) = cache.versions(&d.package, &d.selector, true) {
                        return eligible.to_vec();
                    }
                }
                d.versions.clone()
            })
            .collect();
        for ((_, demand), domain) in edges.iter().zip(&domains) {
            for version in domain {
                if !seen.insert(version) || version == &demand.selected {
                    continue;
                }
                let Some(release) = cache.speculative_release(&demand.package.key(version))? else {
                    continue;
                };
                let after = reusable(&release.manifest)?;
                let improved = before.iter().zip(&after).any(|(was, now)| !*was && *now)
                    && profiles.insert(after);
                let different_root = root && shapes.insert(shape(&release.manifest)?);
                if !improved && !different_root {
                    continue;
                }
                // A different root release can change every selector beneath it.
                // Re-optimize those children conditional on the new root choices.
                let mut next = if root {
                    pins.iter()
                        .filter(|(e, _)| matches!(e.0, super::Owner::Importer(_)))
                        .map(|(e, v)| (e.clone(), v.clone()))
                        .collect()
                } else {
                    pins.clone()
                };
                for ((edge, _), domain) in edges.iter().zip(&domains) {
                    if domain.contains(version) {
                        next.insert((*edge).clone(), version.clone());
                    }
                }
                moves.push(next);
            }
        }
    }
    Ok(moves)
}
