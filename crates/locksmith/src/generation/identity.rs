//! Compute printable snapshot keys after every peer provider is known. A cycle
//! uses bare provider releases; acyclic consumers can use complete provider keys.
use crate::{Value, WorkspaceConfig, graph, manifest, peer, reconcile};
use petgraph::{algo::kosaraju_scc, graph::DiGraph};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum Provider {
    Package(usize),
    Link(String),
    Locked(String),
}

pub(super) struct Identity<'a> {
    pub base: &'a str,
    pub patch: Option<&'a str>,
    pub providers: BTreeMap<String, Provider>,
    pub children: BTreeMap<String, Provider>,
    pub declared: &'a Value,
}

impl Provider {
    fn physical<'a>(&'a self, keys: &'a BTreeMap<usize, String>) -> &'a str {
        match self {
            Self::Package(id) => &keys[id],
            Self::Locked(key) | Self::Link(key) => key,
        }
    }
}

fn locked_target(name: &str, pin: &str, document: &Value) -> Provider {
    if pin.starts_with("link:") {
        Provider::Link(pin.to_owned())
    } else {
        Provider::Locked(graph::snapshot_key(
            name,
            pin,
            graph::section(document, "snapshots"),
        ))
    }
}

fn bindings(
    target: &Provider,
    nodes: &BTreeMap<usize, Identity<'_>>,
    document: &Value,
) -> BTreeMap<String, Provider> {
    match target {
        Provider::Package(id) => nodes[id].providers.clone(),
        Provider::Link(_) => BTreeMap::new(),
        Provider::Locked(key) => {
            let body = &document["snapshots"][key];
            let mut bindings = BTreeMap::new();
            if let Some(tokens) = peer::explicit_bindings(key) {
                for (name, pin) in tokens {
                    if name != "patch_hash" {
                        bindings.insert(name.to_owned(), locked_target(name, pin, document));
                    }
                }
            }
            for name in reconcile::peer_names(document, key) {
                if let Some((_, pin)) = graph::dependency(body, &name) {
                    bindings.insert(name.clone(), locked_target(&name, pin, document));
                }
            }
            bindings
        }
    }
}

// A cyclic peer is printed using its bare release. If two copies bind that
// release in different environments, carry the varying peers into the consumer
// identity. The same distinction may then propagate to an ordinary parent.
pub(super) fn keys(
    nodes: &mut BTreeMap<usize, Identity<'_>>,
    config: &WorkspaceConfig,
    limit: u64,
    document: &Value,
    retained: &BTreeSet<String>,
) -> BTreeMap<usize, String> {
    loop {
        let keys = encode(nodes, config, limit);
        let mut groups = BTreeMap::<&str, Vec<usize>>::new();
        for (&id, key) in &keys {
            groups.entry(key).or_default().push(id);
        }
        let mut changes = BTreeMap::<usize, BTreeMap<String, Provider>>::new();
        for (key, ids) in groups {
            if ids.len() == 1 && !retained.contains(key) {
                continue;
            }
            let prior = document["snapshots"].get(key);
            let names: BTreeSet<_> = ids
                .iter()
                .flat_map(|id| nodes[id].providers.keys().chain(nodes[id].children.keys()))
                .collect();
            for name in names {
                let mut occurrences: Vec<_> = ids
                    .iter()
                    .filter_map(|id| {
                        let node = &nodes[id];
                        Some((
                            Some(*id),
                            node.providers
                                .get(name)
                                .or_else(|| node.children.get(name))?
                                .clone(),
                        ))
                    })
                    .collect();
                if let Some((_, pin)) = prior.and_then(|body| graph::dependency(body, name)) {
                    occurrences.push((None, locked_target(name, pin, document)));
                }
                let Some((_, first)) = occurrences.first() else {
                    continue;
                };
                if occurrences
                    .iter()
                    .all(|(_, p)| p.physical(&keys) == first.physical(&keys))
                {
                    continue;
                }
                let scopes: Vec<_> = occurrences
                    .iter()
                    .map(|(_, p)| bindings(p, nodes, document))
                    .collect();
                for ((id, _), scope) in occurrences.iter().zip(&scopes) {
                    let Some(id) = id else { continue };
                    let node = &nodes[id];
                    for (alias, peer) in scope {
                        if alias == manifest::selector(node.base).0
                            || node.providers.contains_key(alias)
                            || node.children.contains_key(alias)
                            || node.declared.get(alias).is_some()
                        {
                            continue;
                        }
                        if scopes.iter().any(|other| {
                            other.get(alias).map(|p| p.physical(&keys))
                                != Some(peer.physical(&keys))
                        }) {
                            changes
                                .entry(*id)
                                .or_default()
                                .entry(alias.clone())
                                .or_insert_with(|| peer.clone());
                        }
                    }
                }
            }
        }
        if changes.is_empty() {
            return keys;
        }
        for (id, peers) in changes {
            nodes
                .get_mut(&id)
                .expect("existing instance")
                .providers
                .extend(peers);
        }
    }
}

pub(super) fn encode(
    nodes: &BTreeMap<usize, Identity<'_>>,
    config: &WorkspaceConfig,
    limit: u64,
) -> BTreeMap<usize, String> {
    let mut graph = DiGraph::<usize, ()>::new();
    let indices: BTreeMap<_, _> = nodes.keys().map(|&id| (id, graph.add_node(id))).collect();
    for (&id, node) in nodes {
        for p in node.providers.values() {
            if let Provider::Package(other) = p {
                graph.add_edge(indices[&id], indices[other], ());
            }
        }
    }
    let mut keys: BTreeMap<usize, String> = BTreeMap::new();
    for component in kosaraju_scc(&graph) {
        let members: BTreeSet<_> = component.iter().map(|n| graph[*n]).collect();
        for id in &members {
            let node = &nodes[id];
            let mut bound = BTreeMap::new();
            if let Some(hash) = node.patch {
                bound.insert("patch_hash".into(), hash.to_owned());
            }
            for (name, p) in &node.providers {
                let pin = match p {
                    Provider::Link(link) => link.clone(),
                    Provider::Locked(key) => reconcile::reference(
                        name,
                        if config.resolution.lock.dedupe_peers {
                            crate::graph::package_key(key)
                        } else {
                            key
                        },
                    ),
                    Provider::Package(other) => {
                        let collapsed =
                            config.resolution.lock.dedupe_peers || members.contains(other);
                        reconcile::reference(
                            name,
                            if collapsed {
                                nodes[other].base
                            } else {
                                &keys[other]
                            },
                        )
                    }
                };
                bound.insert(name.clone(), pin);
            }
            keys.insert(
                *id,
                peer::generated_identity(node.base, &bound, config, limit),
            );
        }
    }
    keys
}
