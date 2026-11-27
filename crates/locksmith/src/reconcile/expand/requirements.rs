//! Track inherited peer requirements and the evidence for their providers.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde_json::Map;

use super::super::{PeerChanges, PeerRequirements};
use crate::{
    LockfileError, Value, error::missing, graph, patch::hash_in_key, peer, pin::package_key,
};

pub(super) const EDGES: [&str; 2] = ["dependencies", "optionalDependencies"];
pub(super) type Scope = BTreeMap<String, String>;
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Requirement {
    pub(super) optional: bool,
    pub(super) bindings: ProviderEvidence,
}
pub(super) type Requirements = BTreeMap<String, BTreeMap<String, Requirement>>;

#[derive(Clone, PartialEq, Eq)]
pub(super) enum ProviderEvidence {
    Unknown,
    Shared(ProviderBinding),
    Competing(Vec<ProviderBinding>),
}

impl ProviderEvidence {
    fn bindings(&self) -> &[ProviderBinding] {
        match self {
            Self::Unknown => &[],
            Self::Shared(binding) => std::slice::from_ref(binding),
            Self::Competing(bindings) => bindings,
        }
    }

    fn join(&self, other: &Self) -> Self {
        if self == other {
            return self.clone();
        }
        if matches!(self, Self::Unknown) || matches!(other, Self::Unknown) {
            return Self::Unknown;
        }
        if let (Self::Shared(a), Self::Shared(b)) = (self, other)
            && let Some(binding) = a.common(b)
        {
            return Self::Shared(binding);
        }
        let mut bindings = self.bindings().to_vec();
        for incoming in other.bindings() {
            if let Some((existing, merged)) = bindings
                .iter_mut()
                .find_map(|binding| binding.common(incoming).map(|merged| (binding, merged)))
            {
                *existing = merged;
            } else {
                bindings.push(incoming.clone());
            }
        }
        if bindings.len() == 1 {
            Self::Shared(bindings.pop().expect("one shared provider"))
        } else {
            bindings.sort_unstable_by(|a, b| a.reference().cmp(b.reference()));
            Self::Competing(bindings)
        }
    }
}

/// A flattened peer identity proves a release, not a concrete dependency
/// instance. Keep that evidence when descendants use distinct peer contexts.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum ProviderBinding {
    Instance(String),
    Release(String),
}

impl ProviderBinding {
    fn new(reference: &str) -> Self {
        if reference.starts_with("link:") || reference.contains('(') {
            Self::Instance(reference.into())
        } else {
            Self::Release(reference.into())
        }
    }

    pub(super) fn reference(&self) -> &str {
        match self {
            Self::Instance(value) | Self::Release(value) => value,
        }
    }

    fn common(&self, other: &Self) -> Option<Self> {
        if self == other {
            return Some(self.clone());
        }
        let a = self.reference();
        let b = other.reference();
        (!a.starts_with("link:")
            && !b.starts_with("link:")
            && package_key(a) == package_key(b)
            && hash_in_key(a) == hash_in_key(b))
        .then(|| {
            Self::Release(hash_in_key(a).map_or_else(
                || package_key(a).into(),
                |patch| format!("{}(patch_hash={patch})", package_key(a)),
            ))
        })
    }
}

pub(super) fn references(body: &Value) -> impl Iterator<Item = (&str, &str, &str)> {
    EDGES.into_iter().flat_map(move |group| {
        graph::section(body, group)
            .into_iter()
            .flatten()
            .map(move |(name, value)| {
                (
                    group,
                    name.as_str(),
                    value.as_str().expect("selected dependency reference"),
                )
            })
    })
}

pub(super) fn providers(body: &Value) -> Scope {
    let scope = references(body)
        .map(|(_, name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    match peer::canonical_providers(&scope) {
        Cow::Borrowed(_) => scope,
        Cow::Owned(canonical) => canonical,
    }
}

/// Only a newly introduced, competing provider needs range selection. Existing
/// bindings can reflect a permissive installation policy and remain evidence.
pub(super) fn select_provider(
    key: &str,
    name: &str,
    candidates: &[ProviderBinding],
    snapshots: &Map<String, Value>,
    packages: &Map<String, Value>,
    required: &Requirements,
) -> Option<ProviderBinding> {
    let mut pending = vec![key.to_owned()];
    let mut seen = HashSet::new();
    let mut ranges = BTreeSet::new();
    while let Some(key) = pending.pop() {
        if !seen.insert(key.clone()) {
            continue;
        }
        let package = &packages[package_key(&key)];
        let own = package.get("peerDependencies");
        if let Some(range) = own
            .and_then(|peers| peers.get(name))
            .and_then(Value::as_str)
        {
            ranges.insert(range);
        }
        let body = &snapshots[&key];
        if references(body).any(|(_, edge, value)| {
            peer::canonical_binding(edge, value).0 == name
                && own.is_none_or(|peers| peers.get(edge).is_none())
        }) {
            continue;
        }
        for (_, dependency, reference) in references(body) {
            if reference.starts_with("link:")
                || own.is_some_and(|peers| peers.get(dependency).is_some())
            {
                continue;
            }
            let child = graph::snapshot_key(dependency, reference, Some(snapshots));
            if required
                .get(&child)
                .is_some_and(|needs| needs.contains_key(name))
                || packages[package_key(&child)]["peerDependencies"]
                    .get(name)
                    .is_some()
                || snapshots[&child]["transitivePeerDependencies"]
                    .as_array()
                    .is_some_and(|names| names.iter().any(|n| n == name))
            {
                pending.push(child);
            }
        }
    }
    if ranges.is_empty() {
        return None;
    }
    let ranges: Vec<_> = ranges
        .into_iter()
        .map(str::parse::<node_semver::Range>)
        .collect::<Result<_, _>>()
        .ok()?;
    let mut compatible = candidates.iter().filter(|binding| {
        package_key(peer::canonical_binding(name, binding.reference()).1)
            .parse::<node_semver::Version>()
            .is_ok_and(|version| ranges.iter().all(|range| version.satisfies(range)))
    });
    let first = compatible.next()?;
    compatible.next().is_none().then(|| first.clone())
}

fn retired_requirements(
    supplied: &PeerRequirements,
    snapshots: &Map<String, Value>,
    packages: &Map<String, Value>,
    parents: &HashMap<String, Vec<(&str, &str)>>,
) -> PeerRequirements {
    let names: BTreeSet<_> = supplied.values().flatten().collect();
    let mut retired = PeerRequirements::new();
    for name in names {
        // Find consumers still requiring an enclosing provider. Reverse
        // propagation stops at an ordinary local provider or the package itself.
        let mut required: BTreeSet<&str> = snapshots
            .keys()
            .filter(|key| {
                packages[package_key(key)]
                    .get("peerDependencies")
                    .is_some_and(|own| own.get(name).is_some())
            })
            .map(String::as_str)
            .collect();
        let mut pending: Vec<_> = required.iter().copied().collect();
        while let Some(child) = pending.pop() {
            for (parent, dependency) in parents.get(child).into_iter().flatten() {
                let package = &packages[package_key(parent)];
                if package
                    .get("peerDependencies")
                    .is_some_and(|own| own.get(*dependency).is_some())
                    || package_key(parent)
                        .rsplit_once('@')
                        .is_some_and(|(own, _)| own == name)
                    || references(&snapshots[*parent])
                        .any(|(_, edge, value)| peer::canonical_binding(edge, value).0 == name)
                {
                    continue;
                }
                if required.insert(parent) {
                    pending.push(parent);
                }
            }
        }
        let mut pending: Vec<_> = supplied
            .iter()
            .filter(|(_, names)| names.contains(name))
            .map(|(key, _)| key.as_str())
            .collect();
        let mut seen = BTreeSet::new();
        while let Some(key) = pending.pop() {
            if required.contains(key) || !seen.insert(key) {
                continue;
            }
            retired.entry(key.into()).or_default().insert(name.clone());
            for (parent, dependency) in parents.get(key).into_iter().flatten() {
                if packages[package_key(parent)]
                    .get("peerDependencies")
                    .is_none_or(|own| own.get(*dependency).is_none())
                {
                    pending.push(parent);
                }
            }
        }
    }
    retired
}

pub(super) struct Propagation {
    pub(super) required: Requirements,
    pub(super) affected: PeerRequirements,
    pub(super) retired: PeerRequirements,
}

fn initial_requirements(
    additions: &PeerRequirements,
    unresolved: &PeerRequirements,
    snapshots: &Map<String, Value>,
    packages: &Map<String, Value>,
    peers: &mut peer::Merger<'_>,
    source: &impl Fn(&str) -> Option<usize>,
) -> Result<Requirements, LockfileError> {
    let mut required = Requirements::new();
    for (key, names) in additions {
        let side = peers
            .origin_side(key)
            .or_else(|| source(key))
            .ok_or_else(|| missing(&["snapshots", key], "missing deferred peer origin"))?;
        let own = packages[package_key(key)].get("peerDependencies");
        let bindings = if names.iter().all(|name| {
            own.is_some_and(|own| own.get(name).is_some())
                && references(&snapshots[key]).any(|(_, edge, _)| edge == name)
        }) {
            BTreeMap::new()
        } else {
            peers
                .resolution_context(side, key, &providers(&snapshots[key]))
                .ok_or_else(|| {
                    missing(
                        &["snapshots", key],
                        "deferred peer bindings cannot be reconstructed",
                    )
                })?
        };
        let entries = required.entry(key.clone()).or_default();
        for name in names {
            let Some(optional) = peer::optional_requirement(key, name, snapshots, packages) else {
                // Ordinary edge selection can remove the last consumer of an
                // unresolved transitive peer from the borrowed snapshot.
                continue;
            };
            let repaired = packages[package_key(key)]
                .get("peerDependencies")
                .and_then(|own| own.get(name))
                .and_then(|_| references(&snapshots[key]).find(|(_, edge, _)| *edge == name))
                .map(|(_, _, reference)| reference);
            entries.insert(
                name.clone(),
                Requirement {
                    optional,
                    bindings: repaired
                        .or_else(|| bindings.get(name).map(String::as_str))
                        .filter(|_| {
                            !unresolved
                                .get(key)
                                .is_some_and(|names| names.contains(name))
                        })
                        .map_or(ProviderEvidence::Unknown, |value| {
                            ProviderEvidence::Shared(ProviderBinding::new(value))
                        }),
                },
            );
        }
    }
    Ok(required)
}

pub(super) fn propagate(
    changes: PeerChanges,
    snapshots: &Map<String, Value>,
    packages: &Map<String, Value>,
    peers: &mut peer::Merger<'_>,
    source: &impl Fn(&str) -> Option<usize>,
) -> Result<Propagation, LockfileError> {
    let PeerChanges {
        required: additions,
        mut affected,
        unresolved,
        supplied,
    } = changes;
    let mut required =
        initial_requirements(&additions, &unresolved, snapshots, packages, peers, source)?;
    let mut parents: HashMap<String, Vec<(&str, &str)>> = HashMap::new();
    for (key, body) in snapshots {
        for (_, name, value) in references(body).filter(|(_, _, value)| !value.starts_with("link:"))
        {
            parents
                .entry(graph::snapshot_key(name, value, Some(snapshots)))
                .or_default()
                .push((key, name));
        }
    }
    for (key, names) in additions {
        affected.entry(key).or_default().extend(names);
    }
    let mut queue: VecDeque<_> = affected.keys().cloned().collect();
    let mut queued: HashSet<_> = queue.iter().cloned().collect();
    while let Some(child) = queue.pop_front() {
        queued.remove(&child);
        let needs = required.get(&child).cloned().unwrap_or_default();
        let context = affected[&child].clone();
        for (parent, dependency) in parents.get(&child).into_iter().flatten() {
            let body = &snapshots[*parent];
            let local = (!needs.is_empty()).then(|| providers(body));
            let own = packages[package_key(parent)].get("peerDependencies");
            let inherited = own.is_none_or(|m| m.get(*dependency).is_none());
            let node = required.entry((*parent).into()).or_default();
            let mut changed = false;
            for (name, evidence) in needs.iter().filter(|(name, _)| {
                inherited
                    && local
                        .as_ref()
                        .is_none_or(|local| !local.contains_key(*name))
            }) {
                if let Some(existing) = node.get_mut(name) {
                    let merged = Requirement {
                        optional: existing.optional && evidence.optional,
                        bindings: existing.bindings.join(&evidence.bindings),
                    };
                    changed |= *existing != merged;
                    *existing = merged;
                } else {
                    node.insert(name.clone(), evidence.clone());
                    changed = true;
                }
            }
            let node = affected.entry((*parent).into()).or_default();
            let before = node.len();
            node.extend(context.iter().cloned());
            changed |= node.len() != before;
            if changed && queued.insert((*parent).to_owned()) {
                queue.push_back((*parent).into());
            }
        }
    }
    let retired = retired_requirements(&supplied, snapshots, packages, &parents);
    for (key, names) in &retired {
        affected
            .entry(key.clone())
            .or_default()
            .extend(names.iter().cloned());
        if let Some(requirements) = required.get_mut(key) {
            requirements.retain(|name, _| !names.contains(name));
        }
    }
    // New requirements also change existing descendants that consume those
    // providers. Keep their identities and edges in the same propagation pass.
    let mut queue: VecDeque<_> = affected.keys().cloned().collect();
    let mut queued: HashSet<_> = queue.iter().cloned().collect();
    while let Some(parent) = queue.pop_front() {
        queued.remove(&parent);
        let context = affected[&parent].clone();
        for (_, name, value) in
            references(&snapshots[&parent]).filter(|(_, _, value)| !value.starts_with("link:"))
        {
            let child = graph::snapshot_key(name, value, Some(snapshots));
            let own = packages[package_key(&child)].get("peerDependencies");
            let transitive = snapshots[&child]["transitivePeerDependencies"].as_array();
            let mut inherited = context
                .iter()
                .filter(|name| {
                    own.is_some_and(|peers| peers.get(*name).is_some())
                        || transitive
                            .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(name)))
                })
                .peekable();
            if inherited.peek().is_none() {
                continue;
            }
            let names = affected.entry(child.clone()).or_default();
            let before = names.len();
            names.extend(inherited.cloned());
            if names.len() != before && queued.insert(child.clone()) {
                queue.push_back(child);
            }
        }
    }
    Ok(Propagation {
        required,
        affected,
        retired,
    })
}
