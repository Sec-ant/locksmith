//! Index the graph once. Process up to 64 workspace roots per pass over the
//! condensed DAG so temporary memory stays linear even on very wide workspaces.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{ExternalLink, LinkSource, PeerGroup};
use crate::{DocumentKind, LockfileError, Value, graph};
use petgraph::{
    algo::kosaraju_scc,
    graph::{DiGraph, NodeIndex},
};

pub(super) struct Impact<'a> {
    graph: DiGraph<&'a str, ()>,
    roots: Vec<NodeIndex>,
    direct: Vec<Vec<NodeIndex>>,
    snapshots: BTreeMap<&'a str, NodeIndex>,
    pub external_links: BTreeSet<ExternalLink>,
}

impl<'a> Impact<'a> {
    pub fn build(doc: &'a Value) -> Result<Self, LockfileError> {
        let mut graph = DiGraph::new();
        let mut nodes = |section| -> BTreeMap<&'a str, NodeIndex> {
            let keys: BTreeSet<_> = graph::section(doc, section)
                .into_iter()
                .flatten()
                .map(|(key, _)| key.as_str())
                .collect();
            keys.into_iter()
                .map(|key| (key, graph.add_node(key)))
                .collect()
        };
        let workspaces = nodes("importers");
        let snapshots = nodes("snapshots");
        let links = graph::LinkedProjects::new(doc.get("importers"));
        let roots: Vec<_> = workspaces.values().copied().collect();
        let mut direct = vec![Vec::new(); roots.len()];
        let mut external_links = BTreeSet::new();
        let mut issues = Vec::new();
        for (section, kind, nodes) in [
            ("importers", LinkSource::Importer, &workspaces),
            ("snapshots", LinkSource::Snapshot, &snapshots),
        ] {
            for (&key, &from) in nodes {
                let record = &doc[section][key];
                let is_workspace = kind == LinkSource::Importer;
                let mut edges = Vec::new();
                for (group, name, pin) in graph::references(
                    record,
                    DocumentKind::Project,
                    section,
                    key,
                    true,
                    &mut issues,
                ) {
                    let target = if let Some(relative) = pin.strip_prefix("link:") {
                        let base = if is_workspace { key } else { "." };
                        let id = links.resolve(base, relative);
                        let Some(target) = workspaces.get(id.as_str()) else {
                            external_links.insert(ExternalLink {
                                source_kind: kind,
                                from: key.to_string(),
                                dependency: name.to_string(),
                                target: id,
                            });
                            continue;
                        };
                        *target
                    } else {
                        let id = graph::snapshot_key(name, pin, graph::section(doc, "snapshots"));
                        snapshots[id.as_str()] // validated by inspect()
                    };
                    if is_workspace {
                        direct[from.index()].push(target);
                        // A workspace's devDependencies are entry points only
                        // when that workspace itself is the root of the walk.
                        if group == "devDependencies" {
                            continue;
                        }
                    }
                    edges.push(target);
                }
                edges.sort_unstable();
                edges.dedup();
                for to in edges.into_iter().rev() {
                    graph.add_edge(from, to, ());
                }
            }
        }
        for edges in &mut direct {
            edges.sort_unstable();
            edges.dedup();
        }
        if !issues.is_empty() {
            return Err(LockfileError::InvalidGraph(issues));
        }
        Ok(Self {
            graph,
            roots,
            direct,
            snapshots,
            external_links,
        })
    }

    pub fn attribute(&self, groups: &mut [PeerGroup]) -> usize {
        // One multi-source BFS supplies a shortest witness for every reachable
        // snapshot. Stable node and edge order also breaks equal-length ties.
        let mut previous = vec![None; self.graph.node_count()];
        let mut distance = vec![usize::MAX; self.graph.node_count()];
        let mut queue = VecDeque::new();
        for &root in &self.roots {
            distance[root.index()] = 0;
        }
        for &root in &self.roots {
            for &to in &self.direct[root.index()] {
                if distance[to.index()] == usize::MAX {
                    distance[to.index()] = 1;
                    previous[to.index()] = Some(root);
                    queue.push_back(to);
                }
            }
        }
        while let Some(from) = queue.pop_front() {
            for to in self.graph.neighbors(from) {
                if distance[to.index()] == usize::MAX {
                    distance[to.index()] = distance[from.index()] + 1;
                    previous[to.index()] = Some(from);
                    queue.push_back(to);
                }
            }
        }
        let reachable = self
            .snapshots
            .values()
            .filter(|n| distance[n.index()] != usize::MAX)
            .count();
        if groups.is_empty() {
            return reachable;
        }
        let mut by_node = vec![Vec::new(); self.graph.node_count()];
        for (i, group) in groups.iter_mut().enumerate() {
            let mut witness = None;
            for s in &group.bindings {
                let node = self.snapshots[s.snapshot.as_str()];
                by_node[node.index()].push(i);
                let d = distance[node.index()];
                if d != usize::MAX && witness.is_none_or(|(best, _)| d < best) {
                    witness = Some((d, node));
                }
            }
            if let Some((_, mut node)) = witness {
                let mut path = vec![self.graph[node].to_owned()];
                while let Some(p) = previous[node.index()] {
                    path.push(self.graph[p].to_owned());
                    node = p;
                }
                path.reverse();
                group.example_path = Some(path);
            }
        }
        let components = kosaraju_scc(&self.graph);
        let mut component = vec![0; self.graph.node_count()];
        for (id, nodes) in components.iter().enumerate() {
            for n in nodes {
                component[n.index()] = id;
            }
        }
        let mut group_components = vec![Vec::new(); groups.len()];
        for (node, memberships) in by_node.iter().enumerate() {
            for &group in memberships {
                group_components[group].push(component[node]);
            }
        }
        let mut dag = vec![Vec::new(); components.len()];
        for (id, nodes) in components.iter().enumerate() {
            for &n in nodes {
                dag[id].extend(
                    self.graph
                        .neighbors(n)
                        .map(|to| component[to.index()])
                        .filter(|&to| to != id),
                );
            }
            dag[id].sort_unstable();
            dag[id].dedup();
        }
        for chunk in self.roots.chunks(u64::BITS as usize) {
            let mut masks = vec![0u64; components.len()];
            let mut direct = vec![0u64; groups.len()];
            for (bit, &root) in chunk.iter().enumerate() {
                let flag = 1u64 << bit;
                masks[component[root.index()]] |= flag;
                for &to in &self.direct[root.index()] {
                    masks[component[to.index()]] |= flag;
                    for &group in &by_node[to.index()] {
                        direct[group] |= flag;
                    }
                }
            }
            // kosaraju_scc returns components in reverse topological order.
            for id in (0..dag.len()).rev() {
                let m = masks[id];
                if m != 0 {
                    for &to in &dag[id] {
                        masks[to] |= m;
                    }
                }
            }
            for (index, group) in groups.iter_mut().enumerate() {
                let affected = group_components[index]
                    .iter()
                    .fold(0, |mask, &id| mask | masks[id]);
                for (mut mask, output) in [
                    (affected, &mut group.reachable_workspaces),
                    (direct[index], &mut group.direct_workspaces),
                ] {
                    while mask != 0 {
                        let bit = mask.trailing_zeros() as usize;
                        output.push(self.graph[chunk[bit]].to_owned());
                        mask &= mask - 1;
                    }
                }
            }
        }
        reachable
    }
}
