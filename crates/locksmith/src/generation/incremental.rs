//! Retain an existing package instance when its consumer still offers the same
//! peers. A new importer can also reuse an instance witnessed in the old graph.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::graph::Draft;
use crate::{Value, WorkspaceConfig, graph, manifest, reconcile};

type Scope = BTreeMap<String, String>;

/// Changing an existing, unconditional registry range cannot introduce a
/// previously removed dependency. Its consumers are visible by name in the lock.
/// Other override edits can require the original declarations of every release.
pub(super) fn changed_overrides<'a>(
    old: &'a Value,
    header: &'a Value,
) -> Option<BTreeSet<&'a str>> {
    if manifest::same_override_order(old, header) {
        return Some(BTreeSet::new());
    }
    let before = graph::section(old, "overrides")?;
    let after = graph::section(header, "overrides")?;
    if !before.keys().eq(after.keys()) {
        return None;
    }
    let old_ranges = manifest::unconditional_overrides(old);
    let new_ranges = manifest::unconditional_overrides(header);
    before
        .iter()
        .filter(|(name, v)| after.get(*name) != Some(*v))
        .map(|(name, _)| {
            (old_ranges.contains_key(name.as_str()) && new_ranges.contains_key(name.as_str()))
                .then_some(name.as_str())
        })
        .collect()
}

pub(super) fn observes_override(record: &Value, body: &Value, changed: &BTreeSet<&str>) -> bool {
    changed.iter().any(|name| {
        record["peerDependencies"].get(*name).is_some() || graph::dependency(body, name).is_some()
    })
}

/// Fixed reverse edges and broken references in the input lockfile.
struct Predecessors {
    parents: Vec<Vec<usize>>,
    broken: Vec<bool>,
}

fn predecessors(old: &Value) -> Predecessors {
    let snapshots = graph::section(old, "snapshots");
    let indices: std::collections::HashMap<_, _> = snapshots
        .into_iter()
        .flat_map(|map| map.keys())
        .enumerate()
        .map(|(id, key)| (key.as_str(), id))
        .collect();
    let mut parents = vec![Vec::new(); indices.len()];
    let mut broken = vec![false; indices.len()];
    for (id, (key, body)) in snapshots.into_iter().flatten().enumerate() {
        broken[id] = old["packages"].get(graph::package_key(key)).is_none();
        for group in ["dependencies", "optionalDependencies"] {
            for (name, value) in graph::section(body, group).into_iter().flatten() {
                let Some(pin) = value.as_str().filter(|p| !p.starts_with("link:")) else {
                    continue;
                };
                let child = graph::snapshot_key(name, pin, snapshots);
                if let Some(&child) = indices.get(child.as_str()) {
                    parents[child].push(id);
                } else {
                    broken[id] = true;
                }
            }
        }
    }
    Predecessors { parents, broken }
}

/// Include requirements missing from the old suffix, such as an optional peer
/// newly supplied to an ancestor of a locked package. Normal children shadow
/// providers in the outer scope.
fn requirements(old: &Value) -> BTreeMap<String, BTreeSet<String>> {
    let mut names = BTreeMap::new();
    let mut parents = BTreeMap::<String, Vec<&str>>::new();
    let mut work = VecDeque::new();
    let snapshots = graph::section(old, "snapshots");
    for (key, body) in snapshots.into_iter().flatten() {
        let own = reconcile::peer_names(old, key);
        if !own.is_empty() {
            work.push_back(key.as_str());
        }
        names.insert(key.to_owned(), own);
        let declared = &old["packages"][graph::package_key(key)]["peerDependencies"];
        for group in ["dependencies", "optionalDependencies"] {
            for (name, pin) in graph::section(body, group).into_iter().flatten() {
                if declared.get(name).is_none()
                    && let Some(pin) = pin.as_str().filter(|p| !p.starts_with("link:"))
                {
                    parents
                        .entry(graph::snapshot_key(name, pin, snapshots))
                        .or_default()
                        .push(key);
                }
            }
        }
    }
    let mut queued: BTreeSet<_> = work.iter().copied().collect();
    while let Some(child) = work.pop_front() {
        queued.remove(child);
        for &parent in parents.get(child).into_iter().flatten() {
            let body = &old["snapshots"][parent];
            let missing: Vec<_> = names[child]
                .difference(&names[parent])
                .filter(|n| {
                    graph::dependency(body, n).is_none()
                        && manifest::selector(graph::package_key(parent)).0 != n.as_str()
                })
                .cloned()
                .collect();
            if !missing.is_empty() {
                names
                    .get_mut(parent)
                    .expect("parent snapshot")
                    .extend(missing);
                if queued.insert(parent) {
                    work.push_back(parent);
                }
            }
        }
    }
    names
}

fn scoped<'a>(scope: &'a Scope, root: &'a Scope, name: &str, inherit: bool) -> Option<&'a String> {
    scope
        .get(name)
        .or_else(|| inherit.then(|| root.get(name)).flatten())
}

pub(super) struct Partition<'a> {
    old: &'a Value,
    config: &'a WorkspaceConfig,
    scopes: std::cell::OnceCell<BTreeMap<String, Scope>>,
    predecessors: std::cell::OnceCell<Predecessors>,
    sensitive: std::cell::OnceCell<BTreeMap<String, BTreeSet<String>>>,
}
impl<'a> Partition<'a> {
    pub fn new(old: &'a Value, config: &'a WorkspaceConfig) -> Self {
        Self {
            old,
            config,
            scopes: Default::default(),
            predecessors: Default::default(),
            sensitive: Default::default(),
        }
    }
    /// Recheck the ancestors of every changed or broken historical snapshot.
    pub fn incomplete(&self, mut dirty: impl FnMut(&str, &Value) -> bool) -> BTreeSet<String> {
        let index = self.predecessors.get_or_init(|| predecessors(self.old));
        let snapshots = graph::section(self.old, "snapshots");
        let mut missing = index.broken.clone();
        let mut pending = Vec::new();
        for (id, (key, body)) in snapshots.into_iter().flatten().enumerate() {
            if missing[id] || dirty(key, body) {
                missing[id] = true;
                pending.push(id);
            }
        }
        while let Some(id) = pending.pop() {
            for &parent in &index.parents[id] {
                if !missing[parent] {
                    missing[parent] = true;
                    pending.push(parent);
                }
            }
        }
        snapshots
            .into_iter()
            .flat_map(|map| map.keys())
            .zip(missing)
            .filter(|(_, dirty)| *dirty)
            .map(|(key, _)| key.clone())
            .collect()
    }
    pub fn scopes(&self) -> &BTreeMap<String, Scope> {
        let old = self.old;
        let config = self.config;
        self.scopes.get_or_init(|| {
            let snapshots = graph::section(old, "snapshots");
            graph::section(old, "importers")
                .into_iter()
                .flatten()
                .map(|(id, row)| {
                    let mut scope = Scope::new();
                    for group in manifest::GROUPS {
                        for (name, dep) in graph::section(row, group).into_iter().flatten() {
                            if let Some(pin) = dep["version"].as_str() {
                                let key = if pin.starts_with("link:") {
                                    pin.strip_prefix("link:").map_or_else(
                                        || pin.to_owned(),
                                        |path| {
                                            format!(
                                                "link:{}",
                                                crate::workspace_links::member_target(
                                                    id, path, config
                                                )
                                            )
                                        },
                                    )
                                } else {
                                    graph::snapshot_key(name, pin, snapshots)
                                };
                                scope.insert(name.clone(), key);
                            }
                        }
                    }
                    (id.clone(), scope)
                })
                .collect()
        })
    }
    pub fn sensitive(&self) -> &BTreeMap<String, BTreeSet<String>> {
        self.sensitive.get_or_init(|| requirements(self.old))
    }
    pub fn apply(&self, draft: &mut Draft, incomplete: &BTreeSet<String>) -> bool {
        let old = self.old;
        let config = self.config;
        let scopes = self.scopes();
        let empty = Scope::new();
        let old_root = scopes.get(".").unwrap_or(&empty);
        let new_root: Scope = draft.roots["."]
            .iter()
            .map(|(n, d)| (n.clone(), d.target.clone()))
            .collect();
        let mut root_changed = BTreeSet::new();
        let mut prune = scopes.keys().any(|id| !draft.roots.contains_key(id));
        let witnesses = std::cell::OnceCell::new();
        for (id, deps) in &mut draft.roots {
            let before = scopes.get(id.as_str()).unwrap_or(&empty);
            let after: Scope = deps
                .iter()
                .map(|(n, d)| (n.clone(), d.target.clone()))
                .collect();
            let inherit = id != "." && config.resolution.workspace_root_peers;
            let mut changed: BTreeSet<String> = before
                .keys()
                .chain(after.keys())
                .filter(|n| {
                    scoped(before, old_root, n, inherit) != scoped(&after, &new_root, n, inherit)
                })
                .cloned()
                .collect();
            if inherit {
                changed.extend(
                    root_changed
                        .iter()
                        .filter(|n| !after.contains_key(*n))
                        .cloned(),
                );
            }
            prune |= before
                .iter()
                .any(|(n, p)| !p.starts_with("link:") && after.get(n) != Some(p));
            let mut active = BTreeSet::new();
            loop {
                let initial = active.len();
                for (name, dep) in deps.iter() {
                    if dep.target.starts_with("link:") || active.contains(name) {
                        continue;
                    }
                    if incomplete.contains(&dep.target) {
                        active.insert(name.clone());
                        changed.insert(name.clone());
                        continue;
                    }
                    let same = before.get(name) == Some(&dep.target);
                    if same && changed.is_empty() {
                        continue;
                    }
                    let required = self.sensitive().get(&dep.target);
                    let stable = required.is_some_and(|wanted| {
                        if same {
                            return wanted.is_disjoint(&changed);
                        }
                        if wanted.is_empty() {
                            return true;
                        }
                        if !wanted.is_disjoint(&active) {
                            return false;
                        }
                        let existing = witnesses.get_or_init(|| {
                            let mut by_key = BTreeMap::<&str, Vec<&Scope>>::new();
                            for scope in scopes.values() {
                                for key in scope.values() {
                                    by_key.entry(key).or_default().push(scope);
                                }
                            }
                            by_key
                        });
                        existing
                            .get(dep.target.as_str())
                            .into_iter()
                            .flatten()
                            .any(|other| {
                                wanted.iter().all(|n| {
                                    scoped(
                                        other,
                                        old_root,
                                        n,
                                        config.resolution.workspace_root_peers,
                                    ) == scoped(&after, &new_root, n, inherit)
                                })
                            })
                    });
                    if !stable {
                        active.insert(name.clone());
                        changed.insert(name.clone());
                    }
                }
                if active.len() == initial {
                    break;
                }
            }
            if id == "." {
                root_changed = changed;
            }
            for (name, dep) in deps {
                dep.locked = !active.contains(name);
                let pin = if let Some(path) = dep.target.strip_prefix("link:") {
                    format!(
                        "link:{}",
                        crate::workspace_links::member_relative(id, path, config)
                    )
                } else {
                    reconcile::reference(name, &dep.target)
                };
                let previous = &old["importers"][id][dep.group][name];
                prune |= previous.is_null() && before.contains_key(name);
                draft.document["importers"][id][dep.group][name]["version"] = pin.into();
            }
        }
        prune
    }
}
