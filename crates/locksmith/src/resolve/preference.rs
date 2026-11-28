//! Restrict a second choice of locked pins to the providers involved in a conflict.
use super::{MergeContext, peer_names};
use crate::{LockfileError, Value, model::graph, model::pin, model::schema};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Default)]
pub(super) struct Preferences {
    importers: BTreeMap<String, BTreeSet<String>>,
    snapshots: BTreeSet<String>,
}

pub(super) fn dependency<'a>(doc: &'a Value, id: &str, name: &str) -> Option<&'a str> {
    let importer = doc.get("importers")?.get(id)?;
    schema::DEPENDENCY_GROUPS
        .iter()
        .find_map(|g| importer.get(*g)?.get(name)?.get("version")?.as_str())
}

pub(super) fn ordinary_edges_match(
    doc: &Value,
    key: &str,
    other: &Value,
    candidate: &str,
    preserve_versions: bool,
) -> bool {
    let package = &doc["packages"][pin::package_key(key)];
    if package.get("resolution") != other["packages"][pin::package_key(candidate)].get("resolution")
    {
        return false;
    }
    let current = &doc["snapshots"][key];
    let candidate = &other["snapshots"][candidate];
    for group in ["dependencies", "optionalDependencies"] {
        for (before, after) in [(current, candidate), (candidate, current)] {
            for (name, reference) in graph::section(before, group).into_iter().flatten() {
                if package
                    .get("peerDependencies")
                    .and_then(|p| p.get(name))
                    .is_none()
                    && after
                        .get(group)
                        .and_then(|g| g.get(name))
                        .and_then(Value::as_str)
                        .is_none_or(|next| {
                            preserve_versions
                                && Some(pin::package_key(next))
                                    != reference.as_str().map(pin::package_key)
                        })
                {
                    return false;
                }
            }
        }
    }
    true
}

impl Preferences {
    pub(super) fn contains(&self, id: &str, name: &str) -> bool {
        self.importers
            .get(id)
            .is_some_and(|deps| deps.contains(name))
    }

    pub(super) fn snapshot(&self, key: &str) -> bool {
        self.snapshots.contains(pin::package_key(key))
    }

    // Trace the failed release back to the importer edges that selected it.
    // Allocate the reverse edges only after an unsuccessful merge.
    fn consumers(
        &mut self,
        key: &str,
        docs: [&Value; 3],
        target: usize,
        other: usize,
    ) -> (Vec<(String, String)>, bool) {
        let mut roots = Vec::new();
        let mut changed = false;
        for doc in [docs[target], docs[other]] {
            let Some(records) = graph::section(doc, "snapshots") else {
                continue;
            };
            let mut incoming: HashMap<&str, Vec<&str>> = HashMap::new();
            let mut queue = Vec::new();
            for (parent, body) in records {
                let parent = pin::package_key(parent);
                if parent == pin::package_key(key) {
                    queue.push(parent);
                }
                for group in ["dependencies", "optionalDependencies"] {
                    for (name, dep) in graph::section(body, group).into_iter().flatten() {
                        if let Some(version) = dep.as_str().filter(|v| !v.starts_with("link:")) {
                            let child = graph::snapshot_key(name, version, Some(records));
                            if let Some((child, _)) = records.get_key_value(&child) {
                                incoming
                                    .entry(pin::package_key(child))
                                    .or_default()
                                    .push(parent);
                            }
                        }
                    }
                }
            }
            let mut seen = HashSet::new();
            while let Some(package) = queue.pop() {
                if seen.insert(package) {
                    queue.extend(incoming.get(package).into_iter().flatten().copied());
                    changed |= self.snapshots.insert(package.into());
                }
            }
            for (id, importer) in graph::section(doc, "importers").into_iter().flatten() {
                for group in schema::DEPENDENCY_GROUPS {
                    for (name, dep) in graph::section(importer, group).into_iter().flatten() {
                        if let Some(version) = dep["version"].as_str()
                            && seen.contains(pin::package_key(&graph::snapshot_key(
                                name,
                                version,
                                Some(records),
                            )))
                        {
                            roots.push((id.clone(), name.clone()));
                        }
                    }
                }
            }
        }
        (roots, changed)
    }

    pub(super) fn extend(
        &mut self,
        error: &LockfileError,
        docs: [&Value; 3],
        ctx: &MergeContext<'_>,
    ) -> bool {
        let LockfileError::ResolutionRequired { path, .. } = error else {
            return false;
        };
        let target = ctx.target.index();
        let other = ctx.target.other_index();
        if docs[target] == docs[other] && ctx.configurations[target] == ctx.configurations[other] {
            return false;
        }
        let (mut pending, mut changed) = match path.as_slice() {
            [section, id, name, sub, key, ..] if section == "importers" && sub == "snapshots" => {
                let (mut roots, changed) = self.consumers(key, docs, target, other);
                roots.push((id.clone(), name.clone()));
                (roots, changed)
            }
            [section, id, group, name]
                if section == "importers"
                    && schema::DEPENDENCY_GROUPS.contains(&group.as_str()) =>
            {
                (vec![(id.clone(), name.clone())], false)
            }
            [section, id, name, ..] if section == "importers" && ctx.manifests.contains_key(id) => {
                (vec![(id.clone(), name.clone())], false)
            }
            [section, key, ..] if matches!(section.as_str(), "packages" | "snapshots") => {
                let locator = pin::package_key(key);
                if let (Some(a), Some(b)) = (
                    docs[target]["packages"][locator].get("resolution"),
                    docs[other]["packages"][locator].get("resolution"),
                ) && a != b
                {
                    return false;
                }
                // A matching release on the other branch can already contain
                // the final declarations. Try it before switching its consumers.
                if docs[other]["packages"].get(locator).is_some()
                    && self.snapshots.insert(locator.to_owned())
                {
                    return true;
                }
                self.consumers(key, docs, target, other)
            }
            _ => return false,
        };
        let mut visited = BTreeSet::new();
        while let Some((id, name)) = pending.pop() {
            if !visited.insert((id.clone(), name.clone())) {
                continue;
            }
            let keys = docs.map(|doc| {
                let reference = dependency(doc, &id, &name)?;
                Some(graph::snapshot_key(
                    &name,
                    reference,
                    graph::section(doc, "snapshots"),
                ))
            });
            let old = keys[target].as_deref();
            let available = if let Some(new) = keys[other].as_deref() {
                old.is_none_or(|old| {
                    pin::package_key(old) != pin::package_key(new)
                        || ordinary_edges_match(docs[target], old, docs[other], new, false)
                })
            } else {
                old.is_some_and(|key| docs[other]["packages"].get(pin::package_key(key)).is_some())
            };
            if available {
                changed |= self
                    .importers
                    .entry(id.clone())
                    .or_default()
                    .insert(name.clone());
            }
            for (side, key) in keys.iter().enumerate() {
                let Some(key) = key else {
                    continue;
                };
                for provider in peer_names(docs[side], key) {
                    if dependency(docs[side], &id, &provider).is_some() {
                        pending.push((id.clone(), provider));
                    } else if id != "."
                        && ctx.configuration.resolution.workspace_root_peers
                        && dependency(docs[side], ".", &provider).is_some()
                    {
                        pending.push((".".into(), provider));
                    }
                }
            }
        }
        changed
    }
}
