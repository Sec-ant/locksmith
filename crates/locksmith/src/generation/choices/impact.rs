//! Compare workspace closures jointly. Each bit identifies an importer root.
//! Condensing cycles lets every edge propagate its root set once.
use crate::{DocumentKind, Value, graph, manifest};
use petgraph::{algo::kosaraju_scc, graph::DiGraph};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
};

pub(in crate::generation) struct Impact<'a> {
    old: &'a Value,
    roots: BTreeMap<&'a str, usize>,
    previous: OnceCell<Option<Reach<'a>>>,
}

impl<'a> Impact<'a> {
    pub(in crate::generation) fn new(old: &'a Value) -> Self {
        let roots = graph::section(old, "importers")
            .into_iter()
            .flat_map(|m| m.keys())
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        Self {
            old,
            roots,
            previous: OnceCell::new(),
        }
    }

    pub(super) fn count(&self, next: &Value, unchanged: &[&str]) -> usize {
        if unchanged.is_empty() {
            return 0;
        }
        let old = self.old;
        let previous = self.previous.get_or_init(|| Reach::new(old, &self.roots));
        if let Some(before) = previous
            && let Some(after) = Reach::new(next, &self.roots)
        {
            let mut changed = vec![0_u64; before.words];
            before.compare(after, &mut changed, |section, key| {
                let (a, b) = (&old[section][key], &next[section][key]);
                if section == "snapshots" {
                    a != b
                } else {
                    resolutions_changed(a, b)
                }
            });
            return unchanged
                .iter()
                .filter(|id| {
                    let n = self.roots[**id];
                    (changed[n / 64] >> (n % 64)) & 1 != 0
                })
                .count();
        }
        // A partially broken historical lock can still contain valid, independent roots.
        unchanged
            .iter()
            .filter(|&&id| {
                let roots = BTreeSet::from([id.to_owned()]);
                match (
                    graph::closure(old, DocumentKind::Project, &roots),
                    graph::closure(next, DocumentKind::Project, &roots),
                ) {
                    (Ok(a), Ok(b)) => {
                        a != b
                            || a.snapshots
                                .iter()
                                .any(|k| old["snapshots"][k] != next["snapshots"][k])
                            || a.importers.iter().any(|k| {
                                resolutions_changed(&old["importers"][k], &next["importers"][k])
                            })
                    }
                    _ => false,
                }
            })
            .count()
    }
}

fn resolutions_changed(a: &Value, b: &Value) -> bool {
    manifest::GROUPS.iter().any(|&group| {
        let entries = graph::section(a, group);
        entries.map_or(0, |m| m.len()) != graph::section(b, group).map_or(0, |m| m.len())
            || entries
                .into_iter()
                .flatten()
                .any(|(name, dep)| dep["version"] != b[group][name]["version"])
    })
}

struct Reach<'a> {
    projects: BTreeMap<&'a str, usize>,
    snapshots: BTreeMap<&'a str, usize>,
    sets: Vec<u64>,
    words: usize,
}

impl<'a> Reach<'a> {
    fn new(doc: &'a Value, roots: &BTreeMap<&str, usize>) -> Option<Self> {
        let mut edges = DiGraph::<(), ()>::new();
        let mut nodes = |section| {
            graph::section(doc, section)
                .into_iter()
                .flat_map(|m| m.keys())
                .map(|key| (key.as_str(), edges.add_node(())))
                .collect::<BTreeMap<_, _>>()
        };
        let projects = nodes("importers");
        let snapshots = nodes("snapshots");
        // A linked workspace contributes its production dependencies. Its own root
        // additionally includes its development dependencies.
        let starts: BTreeMap<_, _> = roots
            .keys()
            .filter_map(|&id| {
                let &member = projects.get(id)?;
                let start = edges.add_node(());
                edges.add_edge(start, member, ());
                Some((id, start))
            })
            .collect();
        let mut issues = Vec::new();
        for (section, members) in [("importers", &projects), ("snapshots", &snapshots)] {
            let importer = section == "importers";
            for (&key, &node) in members {
                if !importer {
                    graph::check_package(doc, DocumentKind::Project, key, &mut issues);
                }
                let refs = graph::references(
                    &doc[section][key],
                    DocumentKind::Project,
                    section,
                    key,
                    importer,
                    &mut issues,
                );
                if !issues.is_empty() {
                    return None;
                }
                for (group, name, pin) in refs {
                    let from = if group == "devDependencies" {
                        let Some(&start) = starts.get(key) else {
                            continue;
                        };
                        start
                    } else {
                        node
                    };
                    let to = if let Some(path) = pin.strip_prefix("link:") {
                        let target = graph::linked_project(
                            doc.get("importers"),
                            if importer { key } else { "." },
                            path,
                        );
                        let Some(&member) = projects.get(target.as_str()) else {
                            continue;
                        };
                        member
                    } else {
                        let target =
                            graph::snapshot_key(name, pin, graph::section(doc, "snapshots"));
                        *snapshots.get(target.as_str())?
                    };
                    edges.add_edge(from, to, ());
                }
            }
        }
        let components = kosaraju_scc(&edges);
        let mut which = vec![0; edges.node_count()];
        for (i, nodes) in components.iter().enumerate() {
            for &node in nodes {
                which[node.index()] = i;
            }
        }
        let words = roots.len().div_ceil(64);
        let mut sets = vec![0_u64; components.len() * words];
        for (id, start) in starts {
            let n = roots[id];
            sets[which[start.index()] * words + n / 64] |= 1 << (n % 64);
        }
        // Kosaraju returns components in reverse topological order.
        for (i, nodes) in components.iter().enumerate().rev() {
            let (earlier, current) = sets.split_at_mut(i * words);
            for &node in nodes {
                for other in edges.neighbors(node) {
                    let j = which[other.index()];
                    if j != i {
                        for (out, bits) in earlier[j * words..][..words]
                            .iter_mut()
                            .zip(&current[..words])
                        {
                            *out |= bits;
                        }
                    }
                }
            }
        }
        Some(Self {
            projects: projects
                .into_iter()
                .map(|(key, id)| (key, which[id.index()]))
                .collect(),
            snapshots: snapshots
                .into_iter()
                .map(|(key, id)| (key, which[id.index()]))
                .collect(),
            sets,
            words,
        })
    }

    fn compare(
        &self,
        after: Reach<'_>,
        changed: &mut [u64],
        different: impl Fn(&str, &str) -> bool,
    ) {
        let zero = vec![0; self.words];
        for (section, a, b) in [
            ("importers", &self.projects, &after.projects),
            ("snapshots", &self.snapshots, &after.snapshots),
        ] {
            for (&key, &component) in a {
                let before = &self.sets[component * self.words..][..self.words];
                let next = b
                    .get(key)
                    .map_or(&zero[..], |i| &after.sets[i * self.words..][..self.words]);
                let edited = different(section, key);
                for ((out, left), right) in changed.iter_mut().zip(before).zip(next) {
                    *out |= if edited { left | right } else { left ^ right };
                }
            }
            for (&key, &component) in b {
                if !a.contains_key(key) {
                    for (out, right) in changed
                        .iter_mut()
                        .zip(&after.sets[component * self.words..])
                    {
                        *out |= right;
                    }
                }
            }
        }
    }
}
