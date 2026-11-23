//! Retain an existing package instance when its consumer still offers the same
//! peers. A new importer can also reuse an instance witnessed in the old graph.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use indexmap::IndexMap;

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

pub(super) struct HistoricalSnapshot<'a> {
    pub body: &'a Value,
    pub package: &'a str,
    pub record: Option<&'a Value>,
    pub children: Vec<usize>,
    parents: Vec<(usize, bool)>,
    broken: bool,
}

/// The previous lock's ordinary and peer edges share an index. A missing
/// child is a rebuild seed, and never participates in reachability.
pub(super) struct History<'a> {
    pub snapshots: IndexMap<&'a str, HistoricalSnapshot<'a>>,
}

impl<'a> History<'a> {
    fn new(old: &'a Value) -> Self {
        let snapshots = graph::section(old, "snapshots");
        let mut nodes: IndexMap<_, _> = snapshots
            .into_iter()
            .flatten()
            .map(|(key, body)| {
                let package = graph::package_key(key);
                let record = old["packages"].get(package);
                (
                    key.as_str(),
                    HistoricalSnapshot {
                        body,
                        package,
                        record,
                        children: Vec::new(),
                        parents: Vec::new(),
                        broken: record.is_none(),
                    },
                )
            })
            .collect();
        for id in 0..nodes.len() {
            let body = nodes[id].body;
            let peers = nodes[id]
                .record
                .and_then(|record| record.get("peerDependencies"));
            for (_, name, value) in graph::snapshot_dependencies(body) {
                let Some(pin) = value.as_str().filter(|p| !p.starts_with("link:")) else {
                    continue;
                };
                let child = graph::snapshot_key(name, pin, snapshots);
                if let Some(child) = nodes.get_index_of(child.as_str()) {
                    nodes[id].children.push(child);
                    nodes[child]
                        .parents
                        .push((id, peers.is_none_or(|p| p.get(name).is_none())));
                } else {
                    nodes[id].broken = true;
                }
            }
        }
        Self { snapshots: nodes }
    }

    /// Include peers inherited through ordinary children. A dependency already
    /// supplied by the parent shadows the provider in its outer scope.
    fn requirements(&self, old: &Value) -> BTreeMap<String, BTreeSet<String>> {
        let mut names: Vec<_> = self
            .snapshots
            .iter()
            .map(|(key, _)| reconcile::peer_names(old, key))
            .collect();
        let mut queued: Vec<_> = names.iter().map(|set| !set.is_empty()).collect();
        let mut work: VecDeque<_> = (0..names.len()).filter(|&id| queued[id]).collect();
        while let Some(child) = work.pop_front() {
            queued[child] = false;
            for &(parent, ordinary) in &self.snapshots[child].parents {
                if !ordinary {
                    continue;
                }
                let node = &self.snapshots[parent];
                let missing: Vec<_> = names[child]
                    .difference(&names[parent])
                    .filter(|n| {
                        graph::dependency(node.body, n).is_none()
                            && manifest::selector(node.package).0 != n.as_str()
                    })
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    names[parent].extend(missing);
                    if !std::mem::replace(&mut queued[parent], true) {
                        work.push_back(parent);
                    }
                }
            }
        }
        self.snapshots
            .iter()
            .zip(names)
            .map(|((key, _), names)| ((*key).to_owned(), names))
            .collect()
    }
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
    history: std::cell::OnceCell<History<'a>>,
    sensitive: std::cell::OnceCell<BTreeMap<String, BTreeSet<String>>>,
}
impl<'a> Partition<'a> {
    pub fn new(old: &'a Value, config: &'a WorkspaceConfig) -> Self {
        Self {
            old,
            config,
            scopes: Default::default(),
            history: Default::default(),
            sensitive: Default::default(),
        }
    }
    pub fn history(&self) -> &History<'a> {
        self.history.get_or_init(|| History::new(self.old))
    }

    /// Recheck the ancestors of every changed or broken historical snapshot.
    pub fn incomplete(&self, mut dirty: impl FnMut(&str, &Value) -> bool) -> BTreeSet<String> {
        let history = self.history();
        let mut missing: Vec<_> = history
            .snapshots
            .iter()
            .map(|(key, node)| node.broken || dirty(key, node.body))
            .collect();
        let mut pending: Vec<_> = (0..missing.len()).filter(|&id| missing[id]).collect();
        while let Some(id) = pending.pop() {
            for &(parent, _) in &history.snapshots[id].parents {
                if !std::mem::replace(&mut missing[parent], true) {
                    pending.push(parent);
                }
            }
        }
        history
            .snapshots
            .iter()
            .zip(missing)
            .filter(|(_, dirty)| *dirty)
            .map(|((key, _), _)| (*key).to_owned())
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
        self.sensitive
            .get_or_init(|| self.history().requirements(self.old))
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
        // Members compare their inherited peers with the root's changes, so the
        // root goes first even when an importer id such as `-app` sorts before it.
        let (root, members): (Vec<_>, Vec<_>) =
            draft.roots.iter_mut().partition(|(id, _)| *id == ".");
        for (id, deps) in root.into_iter().chain(members) {
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
                        // The draft root still carries provisional keys. Binding
                        // a root provider may change its nested peer suffix.
                        if !wanted.is_disjoint(&active)
                            || inherit
                                && wanted.iter().any(|name| {
                                    root_changed.contains(name) && !after.contains_key(name)
                                })
                        {
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
