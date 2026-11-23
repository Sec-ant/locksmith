//! Follow a historical dependency path while its consumer can still account
//! for the providers visible on that path. A changed sibling invalidates only
//! children that can observe it as a peer.
use super::{Instances, Provider, Requirements, Scope};
use crate::generation::{
    graph::{Build, Dependencies, Node},
    incremental::Partition,
};
use crate::{Value, graph, manifest, peer, reconcile};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    rc::Rc,
};

type Names = BTreeSet<String>;

/// An optional peer may be visible to a parent without being used by any
/// of its children. A retained child's concrete suffix tells us which of
/// those providers it actually used. Propagate that information back through
/// the active occurrences, including cycles, before printing their keys.
pub(super) fn bound_peers(instances: &Instances<'_, '_>) -> Option<Vec<Names>> {
    if !instances.nodes.iter().any(|n| {
        n.children
            .values()
            .any(|p| matches!(p, Provider::Locked(_)))
    }) {
        return None;
    }
    let mut used = vec![Names::new(); instances.nodes.len()];
    let mut parents = vec![Vec::new(); instances.nodes.len()];
    let mut contexts = BTreeMap::new();
    for (id, occurrence) in instances.nodes.iter().enumerate() {
        if id != occurrence.canonical {
            continue;
        }
        let node = &instances.draft.nodes[&occurrence.key];
        used[id].extend(
            occurrence
                .scope
                .keys()
                .filter(|name| node.record["peerDependencies"].get(*name).is_some())
                .cloned(),
        );
        for child in occurrence.children.values() {
            match child {
                Provider::Package(child) => parents[instances.canonical(*child)].push(id),
                Provider::Locked(key) => {
                    let bound = contexts
                        .entry(key)
                        .or_insert_with(|| peer::explicit_bindings(key));
                    for (name, provider) in &occurrence.scope {
                        let provider_name = match provider {
                            Provider::Package(child) => {
                                manifest::selector(
                                    &instances.draft.nodes[&instances.nodes[*child].key].package,
                                )
                                .0
                            }
                            Provider::Locked(key) => manifest::selector(graph::package_key(key)).0,
                            Provider::Link(_) => name,
                        };
                        // The compact hashed suffix does not expose individual
                        // bindings. In that case keep the existing requirement.
                        if bound.as_ref().is_none_or(|peers| {
                            peers.contains_key(name.as_str()) || peers.contains_key(provider_name)
                        }) {
                            used[id].insert(name.clone());
                        }
                    }
                }
                Provider::Link(_) => {}
            }
        }
    }
    let mut work: Vec<_> = used
        .iter()
        .enumerate()
        .flat_map(|(id, names)| names.iter().map(move |name| (id, name.clone())))
        .collect();
    while let Some((child, name)) = work.pop() {
        for &parent in &parents[child] {
            let occurrence = &instances.nodes[parent];
            let node = &instances.draft.nodes[&occurrence.key];
            if occurrence.scope.contains_key(&name)
                && !node.edges.contains_key(&name)
                && manifest::selector(&node.package).0 != name
                && used[parent].insert(name.clone())
            {
                work.push((parent, name.clone()));
            }
        }
    }
    Some(used)
}

pub(super) struct Context {
    pub same: Names,
    pub retained: Names,
    pub changed: Rc<Names>,
}

pub(super) struct History<'a, 'b> {
    old: &'b Value,
    partition: &'a Partition<'b>,
    verified: HashSet<&'b str>,
    implicit: Names,
}

impl<'a, 'b> History<'a, 'b> {
    pub fn new(
        build: &Build<'a, 'b, '_>,
        document: &Value,
        free: &BTreeMap<String, Requirements>,
    ) -> Option<Self> {
        let kept = graph::section(document, "snapshots").filter(|s| !s.is_empty())?;
        let old = build.old;
        let verified = graph::section(old, "snapshots")?
            .keys()
            .filter(|key| kept.contains_key(*key))
            .map(String::as_str)
            .collect();
        let partition = build.partition();
        // An implicit root provider is absent from the importer's declarations.
        // Only an explicit shadow can establish that its binding was unchanged.
        let mut implicit = Names::new();
        if build.config.resolution.lock.auto_install_peers {
            implicit.extend(free.values().flat_map(|r| r.keys()).cloned());
            implicit.extend(partition.sensitive().values().flatten().cloned());
        }
        Some(Self {
            old,
            partition,
            verified,
            implicit,
        })
    }

    pub fn root(&self, id: &str, deps: &Dependencies, outer: Option<&Context>) -> Option<Context> {
        let before = self.partition.scopes().get(id)?;
        let mut changed =
            outer.map_or_else(|| self.implicit.clone(), |root| (*root.changed).clone());
        let mut same = Names::new();
        for name in before.keys().chain(deps.keys()) {
            if let Some(dep) = deps.get(name)
                && before.get(name) == Some(&dep.target)
            {
                same.insert(name.clone());
                if dep.locked || dep.target.starts_with("link:") {
                    changed.remove(name);
                    continue;
                }
            }
            changed.insert(name.clone());
        }
        Some(Context {
            same,
            changed: Rc::new(changed),
            retained: Names::new(),
        })
    }

    pub fn children(
        &self,
        key: &str,
        node: &Node,
        providers: &Scope,
        outer: &Names,
        free: &BTreeMap<String, Requirements>,
    ) -> Context {
        let before = &self.old["snapshots"][key];
        let snapshots = graph::section(self.old, "snapshots");
        let historical = self.partition.sensitive().get(key);
        let current = free.get(key);
        let mut changed: Names = outer
            .iter()
            .filter(|name| {
                historical.is_some_and(|r| r.contains(*name))
                    || current.is_some_and(|r| r.contains_key(*name))
            })
            .cloned()
            .collect();
        // pnpm may have deduplicated this occurrence into a snapshot originally
        // reached from another importer. Check the providers that snapshot
        // actually bound before reusing its children in this occurrence.
        let declared = &self.old["packages"][&node.package]["peerDependencies"];
        let tokens = peer::explicit_bindings(key);
        for name in historical.into_iter().flatten() {
            let current = match providers.get(name) {
                Some(Provider::Package(_)) => {
                    // Its final peer identity is still being determined.
                    changed.insert(name.clone());
                    continue;
                }
                Some(Provider::Locked(key)) => Some(reconcile::reference(name, key)),
                Some(Provider::Link(link)) => Some(link.clone()),
                None => None,
            };
            let direct = declared
                .get(name)
                .and_then(|_| graph::dependency(before, name).map(|(_, pin)| pin));
            if let Some(pin) = direct {
                if current.as_deref() != Some(pin) {
                    changed.insert(name.clone());
                }
            } else if tokens.as_ref().is_none_or(|tokens| {
                tokens
                    .get(name.as_str())
                    .is_some_and(|&previous| Some(previous) != current.as_deref())
            }) {
                changed.insert(name.clone());
            }
        }
        let mut same = Names::new();
        let mut retained = Names::new();
        changed.insert(manifest::selector(&node.package).0.into());
        for group in ["dependencies", "optionalDependencies"] {
            for (name, _) in graph::section(before, group).into_iter().flatten() {
                if !node.edges.contains_key(name)
                    && node.record["peerDependencies"].get(name).is_none()
                {
                    changed.insert(name.clone());
                }
            }
        }
        for (name, dep) in &node.edges {
            if providers.contains_key(name) {
                // This consumer supplies a peer that the package can also resolve itself.
                changed.insert(name.clone());
                continue;
            }
            let target = before[dep.group][name].as_str().map(|pin| {
                if pin.starts_with("link:") {
                    pin.to_owned()
                } else {
                    graph::snapshot_key(name, pin, snapshots)
                }
            });
            if target.as_ref() == Some(&dep.target) {
                same.insert(name.clone());
                if dep.target.starts_with("link:") {
                    changed.remove(name);
                    continue;
                }
                if self.verified.contains(dep.target.as_str()) {
                    retained.insert(name.clone());
                    changed.remove(name);
                    continue;
                }
            }
            changed.insert(name.clone());
        }
        // Start with the old bindings as a witness, then propagate the changes
        // through peer dependencies between siblings, including cycles.
        loop {
            let invalid: Vec<_> = retained
                .iter()
                .filter(|name| {
                    let target = &node.edges[*name].target;
                    free.get(target)
                        .is_some_and(|r| r.keys().any(|peer| changed.contains(peer)))
                        || self
                            .partition
                            .sensitive()
                            .get(target)
                            .is_some_and(|r| !r.is_disjoint(&changed))
                })
                .cloned()
                .collect();
            if invalid.is_empty() {
                break;
            }
            for name in invalid {
                retained.remove(&name);
                changed.insert(name);
            }
        }
        Context {
            same,
            retained,
            changed: Rc::new(changed),
        }
    }
}
