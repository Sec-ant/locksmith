//! Propagate new external peer requirements after ordinary edges are selected.
mod environment;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use environment::{Context, Environment, Environments, Symbol};

use serde_json::Map;

use super::{PeerChanges, PeerRequirements, missing, reference};
use crate::{LockfileError, Value, WorkspaceConfig, graph, manifest, peer};

const EDGES: [&str; 2] = ["dependencies", "optionalDependencies"];
type Scope = BTreeMap<String, String>;
type State = (Symbol, usize, Context);
type Instance = (Symbol, usize, Symbol);
#[derive(Clone, PartialEq, Eq)]
struct Requirement {
    optional: bool,
    bindings: ProviderEvidence,
}
type Requirements = BTreeMap<String, BTreeMap<String, Requirement>>;

#[derive(Clone, PartialEq, Eq)]
enum ProviderEvidence {
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
enum ProviderBinding {
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

    fn reference(&self) -> &str {
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
            && graph::package_key(a) == graph::package_key(b)
            && peer::patch(a) == peer::patch(b))
        .then(|| {
            Self::Release(peer::patch(a).map_or_else(
                || graph::package_key(a).into(),
                |patch| format!("{}(patch_hash={patch})", graph::package_key(a)),
            ))
        })
    }
}

fn references(body: &Value) -> impl Iterator<Item = (&str, &str, &str)> {
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

fn providers(body: &Value) -> Scope {
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
fn select_provider(
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
        let package = &packages[graph::package_key(&key)];
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
                || packages[graph::package_key(&child)]["peerDependencies"]
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
        graph::package_key(peer::canonical_binding(name, binding.reference()).1)
            .parse::<node_semver::Version>()
            .is_ok_and(|version| ranges.iter().all(|range| version.satisfies(range)))
    });
    let first = compatible.next()?;
    compatible.next().is_none().then(|| first.clone())
}

fn installed(
    roots: impl Iterator<Item = (String, String)>,
    snapshots: &Map<String, Value>,
    packages: &Map<String, Value>,
) -> BTreeSet<String> {
    let mut pending: Vec<_> = roots
        .filter(|(_, value)| !value.starts_with("link:"))
        .map(|(name, value)| graph::snapshot_key(&name, &value, Some(snapshots)))
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(key) = pending.pop() {
        if !seen.insert(key.clone()) {
            continue;
        }
        let body = snapshots.get(&key).expect("assembled dependency snapshot");
        let own = packages[graph::package_key(&key)].get("peerDependencies");
        for (_, name, value) in references(body) {
            if !value.starts_with("link:") && own.is_none_or(|peers| peers.get(name).is_none()) {
                pending.push(graph::snapshot_key(name, value, Some(snapshots)));
            }
        }
    }
    seen
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
                packages[graph::package_key(key)]
                    .get("peerDependencies")
                    .is_some_and(|own| own.get(name).is_some())
            })
            .map(String::as_str)
            .collect();
        let mut pending: Vec<_> = required.iter().copied().collect();
        while let Some(child) = pending.pop() {
            for (parent, dependency) in parents.get(child).into_iter().flatten() {
                let package = &packages[graph::package_key(parent)];
                if package
                    .get("peerDependencies")
                    .is_some_and(|own| own.get(*dependency).is_some())
                    || graph::package_key(parent)
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
                if packages[graph::package_key(parent)]
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

pub(super) fn rebuild(
    document: &mut Value,
    peers: &mut peer::Merger<'_>,
    changes: PeerChanges,
    config: &WorkspaceConfig,
    source: impl Fn(&str) -> Option<usize>,
) -> Result<Vec<String>, LockfileError> {
    let PeerChanges {
        required: additions,
        mut affected,
        unresolved,
        supplied,
    } = changes;
    let mut snapshots = std::mem::take(
        document["snapshots"]
            .as_object_mut()
            .expect("assembled snapshots"),
    );
    let mut importers = std::mem::take(
        document["importers"]
            .as_object_mut()
            .expect("assembled importers"),
    );
    let packages = document["packages"]
        .as_object()
        .expect("assembled packages");
    let roots = |record: &Value| {
        manifest::GROUPS
            .into_iter()
            .flat_map(|group| {
                graph::section(record, group)
                    .into_iter()
                    .flatten()
                    .map(|(name, dep)| {
                        (
                            name.clone(),
                            dep["version"].as_str().expect("selected root").to_owned(),
                        )
                    })
            })
            .collect::<Vec<_>>()
    };
    let global = installed(importers.values().flat_map(&roots), &snapshots, packages);
    let original_roots: BTreeMap<_, _> = importers
        .iter()
        .map(|(id, record)| (id.clone(), roots(record)))
        .collect();
    let mut required = Requirements::new();
    for (key, names) in &additions {
        let side = peers
            .origin_side(key)
            .or_else(|| source(key))
            .ok_or_else(|| missing(&["snapshots", key], "missing deferred peer origin"))?;
        let own = packages[graph::package_key(key)].get("peerDependencies");
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
            let Some(optional) = peer::optional_requirement(key, name, &snapshots, packages) else {
                // Ordinary edge selection can remove the last consumer of an
                // unresolved transitive peer from the borrowed snapshot.
                continue;
            };
            let repaired = packages[graph::package_key(key)]
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
    let mut parents: HashMap<String, Vec<(&str, &str)>> = HashMap::new();
    for (key, body) in &snapshots {
        for (_, name, value) in references(body).filter(|(_, _, value)| !value.starts_with("link:"))
        {
            parents
                .entry(graph::snapshot_key(name, value, Some(&snapshots)))
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
            let own = packages[graph::package_key(parent)].get("peerDependencies");
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
    let retired = retired_requirements(&supplied, &snapshots, packages, &parents);
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
            let child = graph::snapshot_key(name, value, Some(&snapshots));
            let own = packages[graph::package_key(&child)].get("peerDependencies");
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
    let mut instances: HashMap<&str, Vec<&str>> = HashMap::new();
    for key in snapshots.keys() {
        instances
            .entry(graph::package_key(key))
            .or_default()
            .push(key);
    }
    let dependencies = peers.provider_dependencies();
    let mut provider_names: BTreeSet<_> = dependencies.values().flatten().copied().collect();
    provider_names.extend(affected.values().flatten().map(String::as_str));
    let mut environments = Environments::new(provider_names);
    let dependencies: HashMap<_, Vec<_>> = dependencies
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(name, names)| {
            (
                environments.symbol(name),
                names.into_iter().map(|n| environments.symbol(n)).collect(),
            )
        })
        .collect();
    let mut inputs = HashMap::with_capacity(affected.len());
    for (key, names) in &affected {
        let mut observed: Vec<_> = names.iter().map(|name| environments.symbol(name)).collect();
        observed.sort_unstable();
        let Some(original) = snapshots.get(key) else {
            continue;
        };
        let own = graph::section(&packages[graph::package_key(key)], "peerDependencies");
        let edges = references(original)
            .map(|(group, name, reference)| {
                let child = graph::snapshot_key(name, reference, Some(&snapshots));
                let child = match snapshots.get_key_value(&child) {
                    Some((key, _)) => Cow::Borrowed(key.as_str()),
                    None => Cow::Owned(child),
                };
                let unchanged =
                    matches!(child, Cow::Borrowed(_)) && !affected.contains_key(child.as_ref());
                InputEdge {
                    group,
                    name,
                    reference,
                    child,
                    unchanged,
                    peer: own.is_some_and(|own| own.contains_key(name)),
                    can_be_full_key: peers.has_release(graph::package_key(reference)),
                }
            })
            .collect();
        inputs.insert(
            key.as_str(),
            InputSnapshot {
                names,
                original,
                own,
                observed,
                edges,
            },
        );
    }
    let mut resolver = Resolver {
        snapshots: &snapshots,
        instances,
        packages,
        required: &required,
        affected: &affected,
        retired: &retired,
        peers,
        source,
        dedupe: config.resolution.lock.dedupe_peers,
        share_peers: config.resolution.dedupe_peer_dependents,
        share_optional: config.resolution.optional_peer_scope
            == crate::OptionalPeerScope::Workspace,
        auto_install: config.resolution.lock.auto_install_peers,
        global: &global,
        project: BTreeSet::new(),
        scope_id: 0,
        environments,
        inputs: &inputs,
        input_cache: HashMap::new(),
        provider_dependencies: dependencies,
        written: HashMap::new(),
        deferred_contexts: BTreeMap::new(),
        cache: HashMap::new(),
        active: HashSet::new(),
        completed: HashMap::new(),
        back_edges: HashSet::new(),
        pending: BTreeSet::new(),
        selected_providers: HashMap::new(),
    };
    let ids: Vec<_> = importers.keys().cloned().collect();
    for id in ids {
        if !resolver.share_optional {
            resolver.scope_id += 1;
            let root = (id != "." && config.resolution.workspace_root_peers)
                .then(|| original_roots["."].clone());
            resolver.project = installed(
                original_roots[&id]
                    .iter()
                    .cloned()
                    .chain(root.into_iter().flatten()),
                &snapshots,
                packages,
            );
        }
        let roots: Vec<_> = manifest::GROUPS
            .into_iter()
            .flat_map(|group| {
                graph::section(&importers[&id], group)
                    .into_iter()
                    .flatten()
                    .map(move |(name, dep)| {
                        (
                            group,
                            name.clone(),
                            dep["version"].as_str().expect("selected root").to_owned(),
                        )
                    })
            })
            .collect();
        let mut settled = false;
        for _ in 0..=roots.len() {
            let mut scope = Scope::new();
            for importer in [
                (id != "." && config.resolution.workspace_root_peers).then_some("."),
                Some(id.as_str()),
            ]
            .into_iter()
            .flatten()
            {
                for group in manifest::GROUPS {
                    for (name, dep) in graph::section(&importers[importer], group)
                        .into_iter()
                        .flatten()
                    {
                        scope.insert(
                            name.clone(),
                            peer::root_provider(
                                importer,
                                dep["version"].as_str().expect("selected root"),
                            ),
                        );
                    }
                }
            }
            let scope = peer::canonical_providers(&scope);
            let scope = resolver.environments.overlay(
                &resolver.environments.empty(),
                scope
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            );
            let mut changed = false;
            for (group, name, initial) in &roots {
                if initial.starts_with("link:") {
                    continue;
                }
                let key = graph::snapshot_key(name, initial, Some(&snapshots));
                let rebound = resolver.node(&key, &scope).map_err(|error| match error {
                    LockfileError::ResolutionRequired {
                        path,
                        message,
                        request,
                    } => LockfileError::ResolutionRequired {
                        request,
                        path: ["importers".into(), id.clone(), name.clone()]
                            .into_iter()
                            .chain(path)
                            .collect(),
                        message,
                    },
                    error => error,
                })?;
                let rebound = rebound.map_or(key.as_str(), |id| resolver.environments.text(id));
                let next = Value::String(reference(name, rebound));
                let previous = &mut importers[&id][*group][name]["version"];
                changed |= *previous != next;
                *previous = next;
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            return Err(missing(
                &["importers", &id],
                "new peer identities do not reach a stable context",
            ));
        }
    }
    for (key, context) in resolver.deferred_contexts {
        let bindings = context
            .bindings
            .into_iter()
            .map(|(name, value)| {
                (
                    resolver.environments.text(name).to_owned(),
                    resolver.environments.text(value).to_owned(),
                )
            })
            .collect();
        resolver.peers.record_context(
            context.side,
            resolver.environments.text(context.original),
            resolver.environments.text(key),
            bindings,
        );
    }
    let written = resolver.written;
    let mut pending = resolver.pending;
    drop(parents);
    let mut replacements: Vec<_> = written
        .into_iter()
        .filter_map(|(key, instance)| {
            if let Cow::Borrowed(body) = instance.body
                && snapshots
                    .get(&key)
                    .is_some_and(|existing| std::ptr::eq(existing, body))
            {
                return None;
            }
            Some((key, instance.body.into_owned()))
        })
        .collect();
    replacements.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    snapshots.extend(replacements);
    pending.retain(|key| !snapshots.contains_key(key));
    document["snapshots"] = Value::Object(snapshots);
    document["importers"] = Value::Object(importers);
    Ok(pending.into_iter().collect())
}

struct InputSnapshot<'a> {
    names: &'a BTreeSet<String>,
    original: &'a Value,
    own: Option<&'a Map<String, Value>>,
    observed: Vec<Symbol>,
    edges: Vec<InputEdge<'a>>,
}
struct InputEdge<'a> {
    group: &'a str,
    name: &'a str,
    reference: &'a str,
    child: Cow<'a, str>,
    unchanged: bool,
    peer: bool,
    can_be_full_key: bool,
}

struct DeferredContext {
    side: usize,
    original: Symbol,
    bindings: Vec<(Symbol, Symbol)>,
}

struct ResolvedInstance<'a> {
    side: usize,
    original: Symbol,
    // Prefer fewer changed peer names, then fewer changed provider versions.
    changes: Option<(usize, usize)>,
    body: Cow<'a, Value>,
}

struct Resolver<'a, 'm, 'd, F> {
    snapshots: &'a Map<String, Value>,
    instances: HashMap<&'a str, Vec<&'a str>>,
    packages: &'a Map<String, Value>,
    required: &'a Requirements,
    affected: &'a PeerRequirements,
    retired: &'a PeerRequirements,
    peers: &'m mut peer::Merger<'d>,
    source: F,
    dedupe: bool,
    share_peers: bool,
    share_optional: bool,
    auto_install: bool,
    global: &'a BTreeSet<String>,
    project: BTreeSet<String>,
    scope_id: usize,
    environments: Environments,
    inputs: &'a HashMap<&'a str, InputSnapshot<'a>>,
    input_cache: HashMap<State, Symbol>,
    provider_dependencies: HashMap<Symbol, Vec<Symbol>>,
    deferred_contexts: BTreeMap<Symbol, DeferredContext>,
    written: HashMap<String, ResolvedInstance<'a>>,
    cache: HashMap<State, Symbol>,
    active: HashSet<Instance>,
    completed: HashMap<Instance, Symbol>,
    back_edges: HashSet<Instance>,
    pending: BTreeSet<String>,
    selected_providers: HashMap<(Symbol, Symbol), Option<ProviderBinding>>,
}

impl<F: Fn(&str) -> Option<usize>> Resolver<'_, '_, '_, F> {
    fn provider_allowed(&self, key: &str, optional: bool) -> bool {
        if optional {
            (if self.share_optional {
                self.global
            } else {
                &self.project
            })
            .contains(key)
        } else {
            self.auto_install || (self.share_peers && self.global.contains(key))
        }
    }

    fn provider_fits(&mut self, key: &str, scope: &Environment) -> bool {
        self.peers
            .origin_side(key)
            .or_else(|| (self.source)(key))
            .and_then(|side| {
                self.peers
                    .binding_context_with(side, key, || Cow::Owned(self.environments.owned(scope)))
            })
            .is_some_and(|context| {
                context.iter().all(|(name, value)| {
                    name == "patch_hash"
                        || self.environments.get(scope, name).is_none_or(|actual| {
                            actual == value
                                || (self.dedupe
                                    && !actual.starts_with("link:")
                                    && !value.starts_with("link:")
                                    && graph::package_key(actual) == graph::package_key(value)
                                    && peer::patch(actual) == peer::patch(value))
                        })
                })
            })
    }

    fn provider(
        &mut self,
        consumer: &str,
        name: &str,
        evidence: &Requirement,
        scope: &Environment,
        concrete: bool,
    ) -> Result<Option<String>, LockfileError> {
        let selected;
        let binding = match &evidence.bindings {
            ProviderEvidence::Shared(binding) => Some(binding),
            ProviderEvidence::Unknown => None,
            ProviderEvidence::Competing(bindings) => {
                let key = (
                    self.environments.symbol(consumer),
                    self.environments.symbol(name),
                );
                selected = self
                    .selected_providers
                    .entry(key)
                    .or_insert_with(|| {
                        select_provider(
                            consumer,
                            name,
                            bindings,
                            self.snapshots,
                            self.packages,
                            self.required,
                        )
                    })
                    .clone();
                selected.as_ref()
            }
        };
        let Some(binding) = binding else {
            if !evidence.optional && self.auto_install {
                return Err(missing(
                    &["snapshots", consumer, name],
                    "new peer has no unique compatible locked provider",
                ));
            }
            return Ok(None);
        };
        let original = binding.reference();
        let key = graph::snapshot_key(name, original, Some(self.snapshots));
        let known = self.snapshots.contains_key(&key)
            || self.written.contains_key(&key)
            || self.peers.origin_side(&key).is_some()
            || (self.source)(&key).is_some();
        if known && self.provider_allowed(&key, evidence.optional) {
            // Selected instances participate in the same propagation pass as
            // their consumers; rebinding them here would traverse cycles twice.
            if self.snapshots.contains_key(&key) && self.affected.contains_key(&key) {
                return Ok(Some(original.into()));
            }
            if self.provider_fits(&key, scope) {
                return Ok(Some(original.into()));
            }
            let side = self
                .peers
                .origin_side(&key)
                .or_else(|| (self.source)(&key))
                .ok_or_else(|| missing(&["snapshots", &key], "missing peer instance origin"))?;
            let providers = self.environments.owned(scope);
            if let Some(rebound) = self
                .peers
                .recontextualize(side, &key, &providers, &providers)
            {
                return Ok(Some(reference(name, &rebound)));
            }
        }
        let mut selected = None;
        for candidate in self
            .instances
            .get(graph::package_key(&key))
            .cloned()
            .unwrap_or_default()
        {
            if peer::patch(candidate) != peer::patch(&key)
                || !self.provider_allowed(candidate, evidence.optional)
            {
                continue;
            }
            // Transitive identities may prove only a release shared by several
            // descendants. Their concrete edges retain the individual contexts;
            // selecting one here would discard information from those edges.
            if !concrete && matches!(binding, ProviderBinding::Release(_)) {
                return Ok(Some(original.into()));
            }
            if !self.provider_fits(candidate, scope) {
                continue;
            }
            if selected.is_some() {
                return Err(missing(
                    &["snapshots", &key],
                    "multiple locked peer instances satisfy the enclosing context",
                ));
            }
            selected = Some(reference(name, candidate));
        }
        if selected.is_none() && !evidence.optional && self.auto_install {
            return Err(missing(
                &["snapshots", &key],
                "no locked peer instance satisfies the enclosing context",
            ));
        }
        Ok(selected)
    }

    fn bound_reference(
        &mut self,
        name: &str,
        original: &str,
        scope: &Scope,
    ) -> Result<Option<String>, LockfileError> {
        let Some(bound) = peer::bound_reference(name, original, scope) else {
            return Ok(None);
        };
        self.concrete_reference(name, original, bound, |_| scope.clone())
    }

    fn bound_from_scope(
        &mut self,
        name: &str,
        original: &str,
        scope: &Environment,
    ) -> Result<Option<String>, LockfileError> {
        let bound = self
            .environments
            .get(scope, name)
            .map(str::to_owned)
            .or_else(|| {
                let (actual, _) = graph::package_key(original).rsplit_once('@')?;
                self.environments
                    .get(scope, actual)
                    .map(|value| format!("{actual}@{value}"))
            });
        let Some(bound) = bound else { return Ok(None) };
        self.concrete_reference(name, original, bound, |resolver| {
            resolver.environments.owned(scope)
        })
    }

    fn concrete_reference(
        &mut self,
        name: &str,
        original: &str,
        bound: String,
        providers: impl FnOnce(&Self) -> Scope,
    ) -> Result<Option<String>, LockfileError> {
        // A deduplicated ancestor can prove only the provider's version. Its
        // child's existing edge supplies the concrete nested peer context.
        if self.dedupe && !bound.starts_with("link:") && !bound.contains('(') {
            let key = graph::snapshot_key(name, &bound, Some(self.snapshots));
            if !self.snapshots.contains_key(&key) && !self.written.contains_key(&key) {
                if bound == graph::package_key(original) {
                    return Ok(Some(original.into()));
                }
                let old_key = graph::snapshot_key(name, original, Some(self.snapshots));
                let side = self
                    .peers
                    .origin_side(&old_key)
                    .or_else(|| (self.source)(&old_key))
                    .ok_or_else(|| {
                        missing(&["snapshots", &old_key], "missing peer instance origin")
                    })?;
                let providers = providers(self);
                return self
                    .peers
                    .concrete_provider(side, name, original, &bound, &providers)
                    .map(Some)
                    .ok_or_else(|| {
                        missing(
                            &["snapshots", &key],
                            "no concrete locked provider satisfies the peer binding",
                        )
                    });
            }
        }
        Ok(Some(bound))
    }

    fn unchanged_context(&mut self, key: &str, scope: &Environment) -> bool {
        let mut pending = vec![(key.to_owned(), scope.clone())];
        let mut seen = BTreeSet::new();
        while let Some((key, scope)) = pending.pop() {
            if self.retired.contains_key(&key) {
                return false;
            }
            let Some(names) = self.affected.get(&key) else {
                continue;
            };
            let state = (
                key.clone(),
                self.environments
                    .project(&scope, &self.inputs[key.as_str()].observed),
            );
            if !seen.insert(state) {
                continue;
            }
            if self
                .required
                .get(&key)
                .is_some_and(|needs| !needs.is_empty())
            {
                return false;
            }
            let body = &self.snapshots[&key];
            let own = self.packages[graph::package_key(&key)].get("peerDependencies");
            let local = self.environments.overlay(
                &scope,
                providers(body)
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            );
            for (_, name, value) in references(body) {
                if names.contains(name)
                    && own.is_some_and(|peers| peers.get(name).is_some())
                    && let Some(bound) = self
                        .environments
                        .get(&scope, name)
                        .map(str::to_owned)
                        .or_else(|| {
                            let (actual, _) = graph::package_key(value).rsplit_once('@')?;
                            self.environments
                                .get(&scope, actual)
                                .map(|value| format!("{actual}@{value}"))
                        })
                    && bound != value
                    && !(self.dedupe && bound == graph::package_key(value))
                {
                    return false;
                }
                if !value.starts_with("link:") {
                    let child = graph::snapshot_key(name, value, Some(self.snapshots));
                    if self.affected.contains_key(&child) {
                        pending.push((child, local.clone()));
                    }
                }
            }
        }
        true
    }

    fn input_context(&mut self, key: &str, scope: &Environment) -> Context {
        self.environments.project_reachable(
            scope,
            &self.inputs[key].observed,
            &self.provider_dependencies,
        )
    }

    fn node(&mut self, key: &str, scope: &Environment) -> Result<Option<Symbol>, LockfileError> {
        // Rebinding can expose a locked provider outside the initially selected
        // graph. Send that reference through the same assembly worklist so its
        // configuration, overrides, metadata and dependency closure are checked.
        let Some(input) = self.inputs.get(key) else {
            if !self.snapshots.contains_key(key) && !self.written.contains_key(key) {
                self.pending.insert(key.into());
            }
            return Ok(None);
        };
        let names = input.names;
        let symbol = self.environments.symbol(key);
        let input_state = (symbol, self.scope_id, self.input_context(key, scope));
        if let Some(resolved) = self.input_cache.get(&input_state) {
            return Ok(Some(*resolved));
        }
        let mut state = (
            symbol,
            self.scope_id,
            self.environments.project(scope, &self.inputs[key].observed),
        );
        let side = self
            .peers
            .origin_side(key)
            .or_else(|| (self.source)(key))
            .ok_or_else(|| missing(&["snapshots", key], "missing peer instance origin"))?;
        let original = input.original;
        let previous = OnceCell::new();
        let known = self.peers.binding_context_with(side, key, || {
            Cow::Borrowed(previous.get_or_init(|| {
                let mut previous = self.environments.owned(scope);
                previous.extend(providers(original));
                previous
            }))
        });
        let previous = previous.into_inner().unwrap_or_default();
        let mut bindings = match known {
            Some(bindings) => bindings,
            None if self.unchanged_context(key, scope) => {
                // Existing opaque identities remain valid when no affected
                // dependency edge or peer requirement changes in their closure.
                self.cache.insert(state, state.0);
                self.input_cache.insert(input_state, state.0);
                return Ok(None);
            }
            None => self
                .peers
                .resolution_context(side, key, &previous)
                .ok_or_else(|| {
                    missing(
                        &["snapshots", key],
                        "peer bindings are ambiguous in the supplied dependency graph",
                    )
                })?,
        };
        let original_key = self.peers.origin_key(key).to_owned();
        let original_identity = self.environments.symbol(&original_key);
        let original_bindings = self.peers.original_context(side, &original_key, &previous);
        let own = input.own;
        // Assembly may already have repaired a provider's version, patch or
        // peer context. Its current edge supersedes the old identity's binding.
        for (_, name, value) in references(original) {
            if own.is_some_and(|peers| peers.contains_key(name))
                && let Some(binding) = bindings.get_mut(name)
                && binding != value
            {
                value.clone_into(binding);
            }
        }
        let mut unbound = BTreeSet::new();
        for name in names {
            if bindings.contains_key(name)
                && let Some(provider) = self.environments.get(scope, name)
            {
                bindings.insert(name.clone(), provider.to_owned());
            }
        }
        for (name, evidence) in self.required.get(key).into_iter().flatten() {
            if let Some(provider) = self.environments.get(scope, name) {
                bindings.insert(name.clone(), provider.to_owned());
                continue;
            }
            let concrete = self.packages[graph::package_key(key)]
                .get("peerDependencies")
                .is_some_and(|peers| peers.get(name).is_some());
            if let Some(binding) = self.provider(key, name, evidence, scope, concrete)? {
                bindings.insert(name.clone(), binding);
                continue;
            }
            bindings.remove(name);
            unbound.insert(name.clone());
        }
        if let Some(retired) = self.retired.get(key) {
            bindings.retain(|name, _| !retired.contains(name));
        }
        // Resolve missing providers before memoization: their concrete context
        // can depend on enclosing peers absent from this node's old identity.
        state.2 = self.environments.overlay_context(
            state.2,
            bindings
                .iter()
                .filter(|(name, _)| names.contains(*name))
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        // A release shared by several descendants deliberately has no concrete
        // instance. Keep its enclosing context in the key until children bind it.
        let mut indirect = Vec::new();
        for name in self
            .required
            .get(key)
            .into_iter()
            .flatten()
            .map(|(name, _)| name)
        {
            let Some(value) = bindings.get(name) else {
                continue;
            };
            if !value.starts_with("link:")
                && !value.contains('(')
                && !self.snapshots.contains_key(&graph::snapshot_key(
                    name,
                    value,
                    Some(self.snapshots),
                ))
            {
                indirect.extend(
                    self.provider_dependencies
                        .get(&self.environments.symbol(name))
                        .into_iter()
                        .flatten()
                        .copied(),
                );
            }
        }
        indirect.sort_unstable();
        indirect.dedup();
        let indirect = self.environments.project(scope, &indirect);
        state.2 = self.environments.combine(state.2, indirect);
        if let Some(resolved) = self.cache.get(&state) {
            self.input_cache.insert(input_state, *resolved);
            return Ok(Some(*resolved));
        }
        let encode = |peers: &mut peer::Merger<'_>, bindings: &Scope| {
            peers
                .context_identity(side, key, bindings, &previous)
                .ok_or_else(|| {
                    missing(
                        &["snapshots", key],
                        "peer identity encoding cannot be verified",
                    )
                })
        };
        let initial = encode(self.peers, &bindings)?;
        let instance = (state.0, self.scope_id, self.environments.symbol(&initial));
        // Once a deduplicated instance is complete, enclosing contexts that
        // produce the same identity reuse its selected dependency record.
        if self.dedupe
            && let Some(resolved) = self.completed.get(&instance)
        {
            self.cache.insert(state, *resolved);
            self.input_cache.insert(input_state, *resolved);
            return Ok(Some(*resolved));
        }
        if !self.active.insert(instance) {
            self.back_edges.insert(instance);
            // The owning frame verifies that the identity stays unchanged.
            self.cache.insert(state, instance.2);
            self.input_cache.insert(input_state, instance.2);
            return Ok(Some(instance.2));
        }
        let edges = &input.edges;
        let mut body = Cow::Borrowed(original);
        for edge in edges {
            let (group, name, value, is_peer) = (edge.group, edge.name, edge.reference, edge.peer);
            if is_peer && unbound.contains(name) {
                body.to_mut()[group]
                    .as_object_mut()
                    .expect("dependency mapping")
                    .remove(name);
                continue;
            }
            if is_peer
                && let Some(bound) = self.bound_reference(name, value, &bindings)?
                && bound != value
            {
                body.to_mut()[group][name] = Value::String(bound);
            }
        }
        if side == super::PUBLISHED {
            for (name, requirement) in self.required.get(key).into_iter().flatten() {
                if own.is_some_and(|peers| peers.contains_key(name))
                    && let Some(binding) = bindings.get(name)
                    && EDGES.iter().all(|group| body[*group].get(name).is_none())
                {
                    let group = if requirement.optional {
                        "optionalDependencies"
                    } else {
                        "dependencies"
                    };
                    body.to_mut()[group][name] = binding.clone().into();
                }
            }
        }
        let mut desired = initial.clone();
        let mut settled = false;
        for _ in 0..=edges.len() {
            let inherited = bindings
                .iter()
                .filter(|(n, _)| n.as_str() != "patch_hash")
                .map(|(n, v)| (n.as_str(), v.as_str()));
            let local = if references(&body).any(|(_, n, v)| peer::canonical_binding(n, v).0 != n) {
                let local = providers(&body);
                self.environments.overlay(
                    scope,
                    inherited.chain(local.iter().map(|(n, v)| (n.as_str(), v.as_str()))),
                )
            } else {
                self.environments.overlay(
                    scope,
                    inherited.chain(references(&body).map(|(_, n, v)| (n, v))),
                )
            };
            let mut changed = false;
            for edge in edges {
                let (group, name, initial_ref, is_peer) =
                    (edge.group, edge.name, edge.reference, edge.peer);
                if is_peer && unbound.contains(name) {
                    continue;
                }
                let bound = if is_peer {
                    self.bound_reference(name, initial_ref, &bindings)?
                } else {
                    None
                };
                let mut chosen = bound.as_deref().unwrap_or(initial_ref);
                let from_scope = if is_peer && names.contains(name) {
                    self.bound_from_scope(name, chosen, scope)?
                } else {
                    None
                };
                if let Some(provider) = from_scope.as_deref()
                    && graph::package_key(provider) == graph::package_key(chosen)
                {
                    chosen = provider;
                }
                let child;
                let next = if chosen.starts_with("link:") {
                    chosen
                } else {
                    let unchanged_input = chosen == initial_ref;
                    child = if (!unchanged_input || edge.can_be_full_key)
                        && (self.written.contains_key(chosen)
                            || self.peers.origin_side(chosen).is_some())
                    {
                        Cow::Borrowed(chosen)
                    } else if unchanged_input {
                        Cow::Borrowed(edge.child.as_ref())
                    } else {
                        Cow::Owned(graph::snapshot_key(name, chosen, Some(self.snapshots)))
                    };
                    let unchanged = unchanged_input && edge.unchanged && child == edge.child;
                    let rebound = if unchanged {
                        None
                    } else {
                        self.node(&child, &local)?
                    };
                    let key = rebound.map_or(child.as_ref(), |id| self.environments.text(id));
                    key.strip_prefix(name)
                        .and_then(|s| s.strip_prefix('@'))
                        .unwrap_or(key)
                };
                if is_peer
                    && let Some(binding) = bindings.get_mut(name)
                    && *binding != next
                {
                    next.clone_into(binding);
                }
                if body[group][name].as_str() != Some(next) {
                    changed = true;
                    body.to_mut()[group][name] = Value::String(next.into());
                }
            }
            let next = encode(self.peers, &bindings)?;
            changed |= desired != next;
            desired = next;
            if !changed {
                settled = true;
                break;
            }
        }
        self.active.remove(&instance);
        if !settled || (self.back_edges.remove(&instance) && desired != initial) {
            return Err(missing(
                &["snapshots", key],
                "new peer identities do not reach a stable cycle",
            ));
        }
        let mut transitive: BTreeSet<&str> = original
            .get("transitivePeerDependencies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|value| value.as_str().expect("peer name"))
            .collect();
        transitive.extend(
            self.required
                .get(key)
                .into_iter()
                .flatten()
                .filter(|(name, _)| own.is_none_or(|m| !m.contains_key(*name)))
                .map(|(name, _)| name.as_str()),
        );
        if let Some(retired) = self.retired.get(key) {
            transitive.retain(|name| !retired.contains(*name));
        }
        for group in EDGES {
            if graph::section(&body, group).is_some_and(Map::is_empty) {
                body.to_mut()
                    .as_object_mut()
                    .expect("snapshot mapping")
                    .remove(group);
            }
        }
        if !transitive.is_empty() {
            if !body["transitivePeerDependencies"]
                .as_array()
                .is_some_and(|prev| {
                    prev.iter()
                        .filter_map(Value::as_str)
                        .eq(transitive.iter().copied())
                })
            {
                body.to_mut()["transitivePeerDependencies"] = Value::Array(
                    transitive
                        .into_iter()
                        .map(|n| Value::String(n.into()))
                        .collect(),
                );
            }
        } else if body.get("transitivePeerDependencies").is_some() {
            body.to_mut()
                .as_object_mut()
                .expect("snapshot mapping")
                .remove("transitivePeerDependencies");
        }
        let identity = self.environments.symbol(&desired);
        if let Some(ResolvedInstance { body: existing, .. }) = self.written.get_mut(&desired)
            && (existing.get("optional").and_then(Value::as_bool) != Some(true)
                || body.get("optional").and_then(Value::as_bool) != Some(true))
        {
            // An instance is optional only when every consumer is optional.
            for record in [existing, &mut body] {
                if record.get("optional").is_some() {
                    record
                        .to_mut()
                        .as_object_mut()
                        .expect("snapshot mapping")
                        .remove("optional");
                }
            }
        }
        let name_changes = original_bindings.as_ref().map(|before| {
            let added = bindings
                .keys()
                .filter(|name| !before.contains_key(*name))
                .count();
            let value_changes = bindings
                .iter()
                .filter(|(name, value)| {
                    before.get(*name).is_some_and(|old| {
                        if self.dedupe {
                            graph::package_key(old) != graph::package_key(value)
                                || peer::patch(old) != peer::patch(value)
                        } else {
                            old != *value
                        }
                    })
                })
                .count();
            (before.len() + 2 * added - bindings.len(), value_changes)
        });
        let replace = self
            .written
            .get(&desired)
            .map_or(
                Some(true),
                |ResolvedInstance {
                     side: old_side,
                     original,
                     changes: old_name_changes,
                     body: existing,
                 }| {
                    // Preserve an already locked instance when another context
                    // converges on its identity. Both bodies have been repaired
                    // under the final configuration; source preference still wins
                    // when they originate on different branches.
                    let old_exact = *original == identity;
                    let new_exact = original_identity == identity;
                    if *old_side == side && old_exact != new_exact {
                        return Some(new_exact);
                    }
                    if *old_side == side
                        && let (Some(old_changes), Some(changes)) = (old_name_changes, name_changes)
                        && *old_changes != changes
                    {
                        return Some(changes < *old_changes);
                    }
                    self.peers.prefer_record(*old_side, existing, side, &body)
                },
            )
            .ok_or_else(|| {
                missing(
                    &["snapshots", &desired],
                    "new peer contexts produce different dependency records",
                )
            })?;
        if replace {
            self.written.insert(
                desired.clone(),
                ResolvedInstance {
                    side,
                    original: original_identity,
                    changes: name_changes,
                    body,
                },
            );
            // Keep input provenance stable while other roots still read it.
            if self.snapshots.contains_key(&desired) {
                let bindings = bindings
                    .iter()
                    .map(|(name, value)| {
                        (
                            self.environments.symbol(name),
                            self.environments.symbol(value),
                        )
                    })
                    .collect();
                self.deferred_contexts.insert(
                    identity,
                    DeferredContext {
                        side,
                        original: original_identity,
                        bindings,
                    },
                );
            } else {
                self.peers
                    .record_context(side, &original_key, &desired, bindings);
            }
        }
        let resolved = self.environments.symbol(&desired);
        self.cache.insert(state, resolved);
        self.input_cache.insert(input_state, resolved);
        if self.dedupe {
            self.completed.insert(instance, resolved);
        }
        Ok(Some(resolved))
    }
}
