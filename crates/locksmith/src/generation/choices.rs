//! Version choices are per edge. Independent selectors for one package share the
//! smallest set of eligible versions, subject to admissible incumbent pins.
use std::collections::{BTreeMap, BTreeSet};

use crate::{PeerIssue, Value, graph, manifest};

mod cover;
mod impact;
pub(super) use impact::Impact;
mod upstream;
pub(super) use upstream::reuse_children;

pub(super) fn introduced_duplicates(doc: &Value, old: &Value) -> BTreeSet<String> {
    let mut names = BTreeMap::<&str, Vec<&str>>::new();
    for key in graph::section(doc, "packages")
        .into_iter()
        .flat_map(|p| p.keys())
    {
        names
            .entry(manifest::selector(key).0)
            .or_default()
            .push(key);
    }
    names
        .into_values()
        .filter(|keys| keys.len() > 1)
        .flatten()
        .filter(|&key| old["packages"].get(key).is_none())
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum Owner {
    Importer(String),
    Snapshot(String),
}

pub(super) type Edge = (Owner, String);
pub(super) type Choices = BTreeMap<Edge, String>;
pub(super) type Demands = BTreeMap<Edge, std::rc::Rc<Demand>>;

/// Keep one parent assignment per decision level. A package can have thousands
/// of eligible releases. Materialize a child assignment only when visiting it.
#[derive(Default)]
pub(super) struct Agenda(Vec<Frame>);

enum Frame {
    Candidate(Choices),
    Alternatives {
        base: Choices,
        edges: Vec<(Edge, Vec<String>)>,
    },
    Equalities {
        base: Choices,
        classes: Vec<(BTreeSet<Edge>, Vec<String>)>,
        indices: Vec<usize>,
    },
}

impl Agenda {
    pub fn candidate(&mut self, pins: Choices) {
        self.0.push(Frame::Candidate(pins));
    }

    pub fn complete_roots(&mut self, pins: Choices, demands: &Demands) {
        if !pins.is_empty()
            && pins
                .keys()
                .all(|(owner, _)| matches!(owner, Owner::Importer(_)))
        {
            let completion = cover(demands, &pins);
            if !completion.is_empty() {
                let mut candidate = pins;
                candidate.extend(completion);
                self.candidate(candidate);
            }
        }
    }

    pub fn alternatives(&mut self, base: &Choices, edges: Vec<(Edge, Vec<String>)>) {
        if !edges.is_empty() {
            self.0.push(Frame::Alternatives {
                base: base.clone(),
                edges,
            });
        }
    }

    /// All consumers of one printed snapshot must agree on its ordinary
    /// children. Merge overlapping equalities and enumerate the product of
    /// the common domains, without rebuilding the graph between equalities.
    pub fn equalities(&mut self, base: &Choices, equalities: Vec<(Vec<Edge>, Vec<String>)>) {
        let mut classes = Vec::<(BTreeSet<Edge>, Vec<String>)>::new();
        for (edges, mut domain) in equalities {
            let mut members: BTreeSet<_> = edges.into_iter().collect();
            let mut i = 0;
            while i < classes.len() {
                if members.is_disjoint(&classes[i].0) {
                    i += 1;
                } else {
                    let (other, eligible) = classes.swap_remove(i);
                    members.extend(other);
                    domain.retain(|v| eligible.contains(v));
                }
            }
            if domain.is_empty() {
                return;
            }
            classes.push((members, domain));
        }
        if !classes.is_empty() {
            let indices = classes.iter().map(|(_, domain)| domain.len() - 1).collect();
            self.0.push(Frame::Equalities {
                base: base.clone(),
                classes,
                indices,
            });
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn next(&mut self) -> Option<Choices> {
        while let Some(frame) = self.0.last_mut() {
            if let Frame::Equalities {
                base,
                classes,
                indices,
            } = frame
            {
                let mut choices = base.clone();
                for ((members, versions), i) in classes.iter().zip(indices.iter()) {
                    for edge in members {
                        choices.insert(edge.clone(), versions[*i].clone());
                    }
                }
                if let Some(i) = indices.iter().rposition(|v| *v != 0) {
                    indices[i] -= 1;
                    for (v, (_, domain)) in indices[i + 1..].iter_mut().zip(&classes[i + 1..]) {
                        *v = domain.len() - 1;
                    }
                } else {
                    self.0.pop();
                }
                return Some(choices);
            }
            if let Frame::Alternatives { base, edges } = frame
                && let Some((edge, versions)) = edges.last_mut()
            {
                if let Some(version) = versions.pop() {
                    let mut next = base.clone();
                    next.insert(edge.clone(), version);
                    return Some(next);
                }
                edges.pop();
                continue;
            }
            if let Some(Frame::Candidate(pins)) = self.0.pop() {
                return Some(pins);
            }
        }
        None
    }
}

#[derive(Clone)]
pub(super) struct Demand {
    pub package: super::RegistryPackage,
    pub selector: String,
    pub versions: Vec<String>,
    pub selected: String,
    pub natural: Option<String>,
    pub incumbent: bool,
}

/// A candidate reproduces this verified graph if every observed selection returns the
/// same release. Assignments on unreachable edges have no effect. A repeated lookup
/// with conflicting defaults requires the same explicit assignment on both traces.
#[derive(Default)]
pub(super) struct Trace {
    pub pins: Choices,
    pub demands: Demands,
}
impl Trace {
    pub fn matches(&self, pins: &Choices) -> bool {
        self.pins.keys().chain(pins.keys()).all(|edge| {
            let (a, b) = (self.pins.get(edge), pins.get(edge));
            if a == b {
                return true;
            }
            let Some(demand) = self.demands.get(edge) else {
                return true;
            };
            match (a, b) {
                (Some(v), None) | (None, Some(v)) => demand.natural.as_ref() == Some(v),
                _ => false,
            }
        })
    }
}

pub(super) fn cover(demands: &Demands, fixed: &Choices) -> Choices {
    let mut groups = BTreeMap::<&super::RegistryPackage, Vec<(&Edge, &Demand)>>::new();
    for (edge, d) in demands {
        groups.entry(&d.package).or_default().push((edge, d));
    }
    let mut choices = BTreeMap::new();
    for group in groups.values() {
        let selected: BTreeSet<_> = group
            .iter()
            .filter(|(e, d)| d.incumbent || fixed.contains_key(*e) || d.versions.len() == 1)
            .map(|(_, d)| d.selected.as_str())
            .collect();
        let domains: BTreeSet<_> = group
            .iter()
            .filter(|(_, d)| !d.versions.iter().any(|v| selected.contains(v.as_str())))
            .map(|(_, d)| d.versions.as_slice())
            .collect();
        let mut allowed = selected;
        allowed.extend(cover::minimum(&domains.into_iter().collect::<Vec<_>>()));
        for (edge, d) in group {
            if !(d.incumbent || fixed.contains_key(*edge)) {
                let v = d
                    .versions
                    .iter()
                    .find(|v| allowed.contains(v.as_str()))
                    .expect("covered nonempty domain");
                if v != &d.selected {
                    choices.insert((*edge).clone(), v.clone());
                }
            }
        }
    }
    choices
}

/// Field order defines the lexicographic objective, including branch bounds.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Score {
    affected_importers: usize,
    changed_edges: usize,
    extra_releases: usize,
    extra_instances: usize,
    new_peer_issues: usize,
}

pub(super) fn score(doc: &Value, old: &Value, issues: &[PeerIssue], impact: &Impact<'_>) -> Score {
    let mut unchanged = Vec::new();
    let mut edits = 0;
    for (id, importer) in graph::section(old, "importers").into_iter().flatten() {
        let current = &doc["importers"][id];
        if current.is_null() {
            continue;
        }
        let mut same_declarations = true;
        for group in manifest::GROUPS {
            let entries = graph::section(importer, group);
            same_declarations &= entries.map(|m| m.len()).unwrap_or(0)
                == graph::section(current, group).map(|m| m.len()).unwrap_or(0);
            for (name, dep) in entries.into_iter().flatten() {
                let after = &current[group][name];
                same_declarations &= dep["specifier"] == after["specifier"];
                edits += usize::from(dep["version"] != after["version"]);
            }
        }
        if same_declarations {
            unchanged.push(id.as_str());
        }
    }
    let touched = impact.count(doc, &unchanged);
    for (key, snapshot) in graph::section(old, "snapshots").into_iter().flatten() {
        let next = &doc["snapshots"][key];
        edits += graph::snapshot_dependencies(snapshot)
            .filter(|(group, name, value)| next[group].get(*name) != Some(*value))
            .count();
    }
    let packages = graph::section(doc, "packages");
    let count = packages.map_or(0, |p| p.len());
    let names: BTreeSet<_> = packages
        .into_iter()
        .flat_map(|p| p.keys())
        .map(|k| manifest::selector(k).0)
        .collect();
    Score {
        new_peer_issues: issues.len(),
        affected_importers: touched,
        changed_edges: edits,
        extra_releases: count - names.len(),
        extra_instances: graph::section(doc, "snapshots")
            .map_or(0, |s| s.len())
            .saturating_sub(count),
    }
}

/// Locate all upstream release decisions that could remove or change a conflicting
/// provider, including a parent whose different version drops an entire subtree.
pub(super) fn ancestors(
    draft: &super::graph::Draft,
    targets: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut parents = BTreeMap::<&str, BTreeSet<&str>>::new();
    for node in draft.nodes.values() {
        for dep in node.edges.values() {
            parents
                .entry(graph::package_key(&dep.target))
                .or_default()
                .insert(&node.package);
        }
    }
    let mut reached = targets.clone();
    let mut todo = targets.iter().cloned().collect::<Vec<_>>();
    while let Some(key) = todo.pop() {
        for &parent in parents.get(key.as_str()).into_iter().flatten() {
            if reached.insert(parent.into()) {
                todo.push(parent.into());
            }
        }
    }
    reached
}

/// Unconditional costs for every compatible graph. Root declarations are
/// active in every solution. Dependencies of a selectable upstream release
/// cannot contribute a global lower bound until that release is forced.
pub(super) fn lower_bound(doc: &Value, old: &Value, demands: &Demands) -> Score {
    // With an old lock the fast path can see only reusable pins, and other
    // published releases might cover more selectors together. Only exact
    // root versions force a release.
    let mut forced = BTreeMap::<&str, BTreeSet<(&super::RegistryPackage, &str)>>::new();
    for (edge, d) in demands {
        if matches!(edge.0, Owner::Importer(_))
            && d.selector.parse::<node_semver::Version>().is_ok()
        {
            forced
                .entry(&d.package.name)
                .or_default()
                .insert((&d.package, &d.selected));
        }
    }
    let duplicates = forced.values().map(|releases| releases.len() - 1).sum();
    let mut edits = 0;
    for (id, importer) in graph::section(old, "importers").into_iter().flatten() {
        let Some(current) = doc["importers"].get(id) else {
            continue;
        };
        for group in manifest::GROUPS {
            for (alias, dep) in graph::section(importer, group).into_iter().flatten() {
                if current[group].get(alias).is_none() {
                    edits += 1;
                    continue;
                }
                let Some(pin) = dep["version"].as_str() else {
                    continue;
                };
                let key = graph::snapshot_key(alias, pin, graph::section(old, "snapshots"));
                let Some((pkg, version)) = super::RegistryPackage::from_key(&key) else {
                    continue;
                };
                let edge = (Owner::Importer(id.clone()), alias.clone());
                if let Some(d) = demands.get(&edge)
                    && (pkg != d.package
                        // The resolver keeps any locked release for `*`, prereleases included.
                        || d.selector != "*"
                            && d.selector.parse::<node_semver::Range>().is_ok()
                            && !manifest::matches_version(&d.selector, Some(version)))
                {
                    edits += 1;
                }
            }
        }
    }
    Score {
        changed_edges: edits,
        extra_releases: duplicates,
        ..Score::default()
    }
}
