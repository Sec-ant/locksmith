//! Rebuild snapshots affected by changes to their peer bindings.
mod environment;
mod requirements;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use environment::{Context, Environment, Environments, Symbol};
use requirements::{
    EDGES, Propagation, ProviderBinding, ProviderEvidence, Requirement, Requirements, Scope,
    propagate, providers, references, select_provider,
};

use serde_json::Map;

use super::{PeerChanges, PeerRequirements};
use crate::{
    LockfileError, Value, WorkspaceConfig,
    error::missing,
    graph,
    patch::hash_in_key,
    peer,
    pin::{package_key, reference},
    schema::DEPENDENCY_GROUPS,
};

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
        let own = packages[package_key(&key)].get("peerDependencies");
        for (_, name, value) in references(body) {
            if !value.starts_with("link:") && own.is_none_or(|peers| peers.get(name).is_none()) {
                pending.push(graph::snapshot_key(name, value, Some(snapshots)));
            }
        }
    }
    seen
}

fn importer_roots(record: &Value) -> Vec<(String, String)> {
    DEPENDENCY_GROUPS
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
}

pub(super) fn rebuild(
    document: &mut Value,
    peers: &mut peer::Merger<'_>,
    changes: PeerChanges,
    config: &WorkspaceConfig,
    source: impl Fn(&str) -> Option<usize>,
) -> Result<Vec<String>, LockfileError> {
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
    let original_roots: BTreeMap<_, _> = importers
        .iter()
        .map(|(id, record)| (id.clone(), importer_roots(record)))
        .collect();
    let global = installed(
        original_roots.values().flatten().cloned(),
        &snapshots,
        packages,
    );
    let Propagation {
        required,
        affected,
        retired,
    } = propagate(changes, &snapshots, packages, peers, &source)?;
    let mut instances: HashMap<&str, Vec<&str>> = HashMap::new();
    for key in snapshots.keys() {
        instances.entry(package_key(key)).or_default().push(key);
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
        let own = graph::section(&packages[package_key(key)], "peerDependencies");
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
                    can_be_full_key: peers.has_release(package_key(reference)),
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
        policy: &config.resolution,
        global: &global,
        project: BTreeSet::new(),
        scope_id: 0,
        environments,
        inputs: &inputs,
        provider_dependencies: dependencies,
        written: HashMap::new(),
        deferred_contexts: BTreeMap::new(),
        pending: BTreeSet::new(),
        traversal: Traversal::default(),
        selected_providers: HashMap::new(),
    };
    resolver.resolve_importers(
        &mut importers,
        &original_roots,
        config.resolution.workspace_root_peers,
    )?;
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

type AddedPeerEdge<'a> = (&'static str, &'a str, String, bool);

#[derive(Clone, Copy)]
struct SnapshotEdges<'pass, 'input> {
    input: &'pass InputSnapshot<'input>,
    added_edges: &'pass [AddedPeerEdge<'input>],
    unbound: &'pass BTreeSet<String>,
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SnapshotContext {
    snapshot: Symbol,
    scope_id: usize,
    provider_context: Context,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct InstanceKey {
    snapshot: Symbol,
    scope_id: usize,
    identity: Symbol,
}

#[derive(Default)]
struct Traversal {
    input_cache: HashMap<SnapshotContext, Symbol>,
    cache: HashMap<SnapshotContext, Symbol>,
    active: HashSet<InstanceKey>,
    completed: HashMap<InstanceKey, Symbol>,
    back_edges: HashSet<InstanceKey>,
}

impl Traversal {
    fn remember(&mut self, state: SnapshotContext, input: SnapshotContext, resolved: Symbol) {
        self.cache.insert(state, resolved);
        self.input_cache.insert(input, resolved);
    }
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
    policy: &'a crate::ResolutionSettings,
    global: &'a BTreeSet<String>,
    project: BTreeSet<String>,
    scope_id: usize,
    environments: Environments,
    inputs: &'a HashMap<&'a str, InputSnapshot<'a>>,
    provider_dependencies: HashMap<Symbol, Vec<Symbol>>,
    deferred_contexts: BTreeMap<Symbol, DeferredContext>,
    written: HashMap<String, ResolvedInstance<'a>>,
    pending: BTreeSet<String>,
    traversal: Traversal,
    selected_providers: HashMap<(Symbol, Symbol), Option<ProviderBinding>>,
}

impl<'a, F: Fn(&str) -> Option<usize>> Resolver<'a, '_, '_, F> {
    fn resolve_importers(
        &mut self,
        importers: &mut Map<String, Value>,
        original_roots: &BTreeMap<String, Vec<(String, String)>>,
        workspace_root_peers: bool,
    ) -> Result<(), LockfileError> {
        let mut ids: Vec<_> = importers.keys().cloned().collect();
        if let Some(pos) = ids.iter().position(|id| id == ".") {
            ids[..=pos].rotate_right(1);
        }
        for id in ids {
            if self.policy.optional_peer_scope != crate::OptionalPeerScope::Workspace {
                self.scope_id += 1;
                let root = (id != "." && workspace_root_peers).then(|| original_roots["."].clone());
                self.project = installed(
                    original_roots[&id]
                        .iter()
                        .cloned()
                        .chain(root.into_iter().flatten()),
                    self.snapshots,
                    self.packages,
                );
            }
            let roots: Vec<_> = DEPENDENCY_GROUPS
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
                    (id != "." && workspace_root_peers).then_some("."),
                    Some(id.as_str()),
                ]
                .into_iter()
                .flatten()
                {
                    for group in DEPENDENCY_GROUPS {
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
                let scope = self.environments.overlay(
                    &self.environments.empty(),
                    scope
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str())),
                );
                let mut changed = false;
                for (group, name, initial) in &roots {
                    if initial.starts_with("link:") {
                        continue;
                    }
                    let key = graph::snapshot_key(name, initial, Some(self.snapshots));
                    let rebound = self.node(&key, &scope).map_err(|error| match error {
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
                    let rebound = rebound.map_or(key.as_str(), |id| self.environments.text(id));
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
        Ok(())
    }

    fn provider_allowed(&self, key: &str, optional: bool) -> bool {
        if optional {
            (if self.policy.optional_peer_scope == crate::OptionalPeerScope::Workspace {
                self.global
            } else {
                &self.project
            })
            .contains(key)
        } else {
            self.policy.lock.auto_install_peers
                || (self.policy.dedupe_peer_dependents && self.global.contains(key))
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
                                || (self.policy.lock.dedupe_peers
                                    && !actual.starts_with("link:")
                                    && !value.starts_with("link:")
                                    && package_key(actual) == package_key(value)
                                    && hash_in_key(actual) == hash_in_key(value))
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
            if !evidence.optional && self.policy.lock.auto_install_peers {
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
            .get(package_key(&key))
            .cloned()
            .unwrap_or_default()
        {
            if hash_in_key(candidate) != hash_in_key(&key)
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
        if selected.is_none() && !evidence.optional && self.policy.lock.auto_install_peers {
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
                let (actual, _) = package_key(original).rsplit_once('@')?;
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
        if self.policy.lock.dedupe_peers && !bound.starts_with("link:") && !bound.contains('(') {
            let key = graph::snapshot_key(name, &bound, Some(self.snapshots));
            if !self.snapshots.contains_key(&key) && !self.written.contains_key(&key) {
                if bound == package_key(original) {
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
            let own = self.packages[package_key(&key)].get("peerDependencies");
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
                            let (actual, _) = package_key(value).rsplit_once('@')?;
                            self.environments
                                .get(&scope, actual)
                                .map(|value| format!("{actual}@{value}"))
                        })
                    && bound != value
                    && !(self.policy.lock.dedupe_peers && bound == package_key(value))
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

    fn bind_providers(
        &mut self,
        key: &str,
        scope: &Environment,
        input: &InputSnapshot<'_>,
        bindings: &mut Scope,
    ) -> Result<BTreeSet<String>, LockfileError> {
        // Assembly may already have repaired a provider's version, patch or
        // peer context. Its current edge supersedes the old identity's binding.
        for (_, name, value) in references(input.original) {
            if input.own.is_some_and(|peers| peers.contains_key(name))
                && let Some(binding) = bindings.get_mut(name)
                && binding != value
            {
                value.clone_into(binding);
            }
        }
        let mut unbound = BTreeSet::new();
        for name in input.names {
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
            let concrete = self.packages[package_key(key)]
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
        Ok(unbound)
    }

    fn finish_snapshot(&self, key: &str, input: &InputSnapshot<'_>, body: &mut Cow<'a, Value>) {
        let mut transitive: BTreeSet<&str> = input
            .original
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
                .filter(|(name, _)| input.own.is_none_or(|m| !m.contains_key(*name)))
                .map(|(name, _)| name.as_str()),
        );
        if let Some(retired) = self.retired.get(key) {
            transitive.retain(|name| !retired.contains(*name));
        }
        for group in EDGES {
            if graph::section(body, group).is_some_and(Map::is_empty) {
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
    }

    fn publish_instance(
        &mut self,
        side: usize,
        original_key: &str,
        original_bindings: Option<&Scope>,
        desired: String,
        bindings: Scope,
        mut body: Cow<'a, Value>,
    ) -> Result<Symbol, LockfileError> {
        let original_identity = self.environments.symbol(original_key);
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
        let name_changes = original_bindings.map(|before| {
            let added = bindings
                .keys()
                .filter(|name| !before.contains_key(*name))
                .count();
            let value_changes = bindings
                .iter()
                .filter(|(name, value)| {
                    before.get(*name).is_some_and(|old| {
                        if self.policy.lock.dedupe_peers {
                            package_key(old) != package_key(value)
                                || hash_in_key(old) != hash_in_key(value)
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
                    .record_context(side, original_key, &desired, bindings);
            }
        }
        Ok(identity)
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
        let input_state = SnapshotContext {
            snapshot: symbol,
            scope_id: self.scope_id,
            provider_context: self.input_context(key, scope),
        };
        if let Some(resolved) = self.traversal.input_cache.get(&input_state) {
            return Ok(Some(*resolved));
        }
        let mut state = SnapshotContext {
            snapshot: symbol,
            scope_id: self.scope_id,
            provider_context: self.environments.project(scope, &self.inputs[key].observed),
        };
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
                self.traversal.remember(state, input_state, state.snapshot);
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
        let original_bindings = self.peers.original_context(side, &original_key, &previous);
        let unbound = self.bind_providers(key, scope, input, &mut bindings)?;
        // Resolve missing providers before memoization: their concrete context
        // can depend on enclosing peers absent from this node's old identity.
        state.provider_context = self.environments.overlay_context(
            state.provider_context,
            bindings
                .iter()
                .filter(|(name, _)| names.contains(*name))
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        let indirect = self.indirect_context(key, scope, &bindings);
        state.provider_context = self.environments.combine(state.provider_context, indirect);
        if let Some(resolved) = self.traversal.cache.get(&state) {
            self.traversal.input_cache.insert(input_state, *resolved);
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
        let instance = InstanceKey {
            snapshot: state.snapshot,
            scope_id: self.scope_id,
            identity: self.environments.symbol(&initial),
        };
        // Once a deduplicated instance is complete, enclosing contexts that
        // produce the same identity reuse its selected dependency record.
        if self.policy.lock.dedupe_peers
            && let Some(&resolved) = self.traversal.completed.get(&instance)
        {
            self.traversal.remember(state, input_state, resolved);
            return Ok(Some(resolved));
        }
        if !self.traversal.active.insert(instance) {
            self.traversal.back_edges.insert(instance);
            // The owning frame verifies that the identity stays unchanged.
            self.traversal
                .remember(state, input_state, instance.identity);
            return Ok(Some(instance.identity));
        }
        let (mut body, added_edges) =
            self.prepare_edges(key, side == super::PUBLISHED, input, &bindings, &unbound)?;
        let edges = SnapshotEdges {
            input,
            added_edges: &added_edges,
            unbound: &unbound,
        };
        let (desired, settled) = self.settle_edges(
            scope,
            edges,
            &mut body,
            &mut bindings,
            initial.clone(),
            encode,
        )?;
        self.traversal.active.remove(&instance);
        if !settled || (self.traversal.back_edges.remove(&instance) && desired != initial) {
            return Err(missing(
                &["snapshots", key],
                "new peer identities do not reach a stable cycle",
            ));
        }
        self.finish_snapshot(key, input, &mut body);
        let resolved = self.publish_instance(
            side,
            &original_key,
            original_bindings.as_deref(),
            desired,
            bindings,
            body,
        )?;
        self.traversal.remember(state, input_state, resolved);
        if self.policy.lock.dedupe_peers {
            self.traversal.completed.insert(instance, resolved);
        }
        Ok(Some(resolved))
    }

    fn indirect_context(&mut self, key: &str, scope: &Environment, bindings: &Scope) -> Context {
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
        self.environments.project(scope, &indirect)
    }

    fn prepare_edges(
        &mut self,
        key: &str,
        published: bool,
        input: &InputSnapshot<'a>,
        bindings: &Scope,
        unbound: &BTreeSet<String>,
    ) -> Result<(Cow<'a, Value>, Vec<AddedPeerEdge<'a>>), LockfileError> {
        let edges = &input.edges;
        let mut body = Cow::Borrowed(input.original);
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
                && let Some(bound) = self.bound_reference(name, value, bindings)?
                && bound != value
            {
                body.to_mut()[group][name] = Value::String(bound);
            }
        }
        let mut added_edges = Vec::new();
        if published {
            for (name, requirement) in self.required.get(key).into_iter().flatten() {
                if input.own.is_some_and(|peers| peers.contains_key(name))
                    && let Some(binding) = bindings.get(name)
                    && EDGES.iter().all(|group| body[*group].get(name).is_none())
                {
                    let group = if requirement.optional {
                        "optionalDependencies"
                    } else {
                        "dependencies"
                    };
                    body.to_mut()[group][name] = binding.clone().into();
                    added_edges.push((
                        group,
                        name.as_str(),
                        binding.clone(),
                        self.peers.has_release(package_key(binding)),
                    ));
                }
            }
        }
        Ok((body, added_edges))
    }

    fn edge_scope(&mut self, scope: &Environment, body: &Value, bindings: &Scope) -> Environment {
        let inherited = bindings
            .iter()
            .filter(|(n, _)| n.as_str() != "patch_hash")
            .map(|(n, v)| (n.as_str(), v.as_str()));
        if references(body).any(|(_, n, v)| peer::canonical_binding(n, v).0 != n) {
            let local = providers(body);
            self.environments.overlay(
                scope,
                inherited.chain(local.iter().map(|(n, v)| (n.as_str(), v.as_str()))),
            )
        } else {
            self.environments.overlay(
                scope,
                inherited.chain(references(body).map(|(_, n, v)| (n, v))),
            )
        }
    }

    fn settle_edges(
        &mut self,
        scope: &Environment,
        edges: SnapshotEdges<'_, 'a>,
        body: &mut Cow<'a, Value>,
        bindings: &mut Scope,
        initial: String,
        encode: impl Fn(&mut peer::Merger<'_>, &Scope) -> Result<String, LockfileError>,
    ) -> Result<(String, bool), LockfileError> {
        let SnapshotEdges {
            input,
            added_edges,
            unbound,
        } = edges;
        let mut desired = initial;
        let mut settled = false;
        for _ in 0..=(input.edges.len() + added_edges.len()) {
            let local = self.edge_scope(scope, body, bindings);
            let mut changed = false;
            for (group, name, initial_ref, is_peer, edge_child, unchanged, can_be_full_key) in input
                .edges
                .iter()
                .map(|edge| {
                    (
                        edge.group,
                        edge.name,
                        edge.reference,
                        edge.peer,
                        Some(edge.child.as_ref()),
                        edge.unchanged,
                        edge.can_be_full_key,
                    )
                })
                .chain(
                    added_edges
                        .iter()
                        .map(|(group, name, initial_ref, can_be_full_key)| {
                            (
                                *group,
                                *name,
                                initial_ref.as_str(),
                                true,
                                None,
                                false,
                                *can_be_full_key,
                            )
                        }),
                )
            {
                if is_peer && unbound.contains(name) {
                    continue;
                }
                let bound = if is_peer {
                    self.bound_reference(name, initial_ref, bindings)?
                } else {
                    None
                };
                let mut chosen = bound.as_deref().unwrap_or(initial_ref);
                let from_scope = if is_peer && input.names.contains(name) {
                    self.bound_from_scope(name, chosen, scope)?
                } else {
                    None
                };
                if let Some(provider) = from_scope.as_deref()
                    && package_key(provider) == package_key(chosen)
                {
                    chosen = provider;
                }
                let child;
                let next = if chosen.starts_with("link:") {
                    chosen
                } else {
                    let unchanged_input = chosen == initial_ref;
                    child = if (!unchanged_input || can_be_full_key)
                        && (self.written.contains_key(chosen)
                            || self.peers.origin_side(chosen).is_some()
                            || self.packages.contains_key(package_key(chosen))
                            || self.peers.has_release(package_key(chosen)))
                    {
                        Cow::Borrowed(chosen)
                    } else if let Some(edge_child) = edge_child.filter(|_| unchanged_input) {
                        Cow::Borrowed(edge_child)
                    } else {
                        Cow::Owned(graph::snapshot_key(name, chosen, Some(self.snapshots)))
                    };
                    let unchanged = unchanged_input
                        && unchanged
                        && edge_child.is_some_and(|c| c == child.as_ref());
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
            let next = encode(self.peers, bindings)?;
            changed |= desired != next;
            desired = next;
            if !changed {
                settled = true;
                break;
            }
        }
        Ok((desired, settled))
    }
}
