//! Bind peer dependencies in the scope of each consumer. The reverse dependency
//! index propagates requirements (including absent optional peers) to a fixed point.
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
mod lineage;

use serde_json::json;

use super::{
    Result,
    graph::{Build, Dependencies, Dependency, Draft},
};
use crate::{Value, graph, manifest, reconcile};

type Requirements = BTreeMap<String, Requirement>;

/// The declarations that require a peer at a package, and how they reach it.
#[derive(Default)]
pub(super) struct Requirement {
    declarations: Vec<usize>,
    /// The package declares this peer itself.
    own: bool,
    /// The dependencies through which a descendant's declaration arrives.
    via: BTreeSet<String>,
}

impl Requirement {
    /// A consumer-provided peer replaces the package's own dependency of that
    /// name, together with the peers that dependency would have required.
    fn active(&self, replaced: impl Fn(&str) -> bool) -> bool {
        self.own || self.via.iter().any(|edge| !replaced(edge))
    }
}

struct PeerDeclaration {
    owner: String,
    range: String,
    optional: bool,
    install_optional: bool,
}

/// A declaration reaches precisely the ancestors with a path on which no
/// package supplies that peer. Walk that path once per declaration, even in
/// cycles. Owners are visited in lockfile-key order, preserving the first
/// historical owner used to select an automatically installed peer.
fn requirements(draft: &Draft) -> (BTreeMap<String, Requirements>, Vec<PeerDeclaration>) {
    let nodes: Vec<_> = draft.nodes.values().collect();
    let indices: BTreeMap<_, _> = draft
        .nodes
        .keys()
        .enumerate()
        .map(|(id, k)| (k.as_str(), id))
        .collect();
    let mut parents = vec![Vec::new(); nodes.len()];
    for (parent, node) in nodes.iter().enumerate() {
        for (name, dep) in &node.edges {
            if let Some(&child) = indices.get(dep.target.as_str()) {
                parents[child].push((parent, name.as_str()));
            }
        }
    }
    let names: Vec<_> = nodes
        .iter()
        .map(|n| manifest::selector(&n.package).0)
        .collect();
    let mut free = Vec::new();
    free.resize_with(nodes.len(), BTreeMap::<&str, Requirement>::new);
    let mut declarations = Vec::new();
    let mut visited = vec![usize::MAX; nodes.len()];
    let mut pending = Vec::new();
    for (source, (key, node)) in draft.nodes.iter().enumerate() {
        for (peer, range) in graph::section(&node.record, "peerDependencies")
            .into_iter()
            .flatten()
        {
            if names[source] == peer {
                continue;
            }
            let id = declarations.len();
            let optional = node.record["peerDependenciesMeta"][peer]["optional"] == true;
            let supplied = node.edges.contains_key(peer);
            declarations.push(PeerDeclaration {
                owner: key.clone(),
                range: range.as_str().expect("peer range").into(),
                optional: supplied || optional,
                install_optional: optional && !supplied,
            });
            pending.push(source);
            visited[source] = id;
            while let Some(child) = pending.pop() {
                let requirement = free[child].entry(peer).or_default();
                requirement.declarations.push(id);
                requirement.own |= child == source;
                for &(parent, edge) in &parents[child] {
                    if names[parent] == peer || nodes[parent].edges.contains_key(peer) {
                        continue;
                    }
                    free[parent]
                        .entry(peer)
                        .or_default()
                        .via
                        .insert(edge.to_owned());
                    if visited[parent] != id {
                        visited[parent] = id;
                        pending.push(parent);
                    }
                }
            }
        }
    }
    let free = draft
        .nodes
        .keys()
        .cloned()
        .zip(free.into_iter().map(|entries| {
            entries
                .into_iter()
                .map(|(name, requirement)| (name.to_owned(), requirement))
                .collect()
        }))
        .collect();
    (free, declarations)
}

use super::identity::{self, Identity, Provider};
type Scope = BTreeMap<String, Provider>;
type ScopeChanges = Option<Rc<BTreeSet<String>>>;

struct Instance {
    key: String,
    scope: Scope,
    children: Scope,
    parent: Option<usize>,
    canonical: usize,
    changed: ScopeChanges,
}

struct Instances<'a, 'b> {
    draft: &'a Draft,
    free: &'a BTreeMap<String, Requirements>,
    nodes: Vec<Instance>,
    memo: BTreeMap<(String, Scope, ScopeChanges), usize>,
    history: Option<lineage::History<'a, 'b>>,
}

impl Instances<'_, '_> {
    fn slots<'a>(
        &mut self,
        deps: impl Iterator<Item = (&'a String, &'a Dependency)>,
        parent: Option<usize>,
        context: Option<&lineage::Context>,
    ) -> Scope {
        deps.map(|(name, dep)| {
            let provider = if dep.target.starts_with("link:") {
                Provider::Link(dep.target.clone())
            } else if dep.locked || context.is_some_and(|ctx| ctx.retained.contains(name)) {
                Provider::Locked(dep.target.clone())
            } else {
                let id = self.nodes.len();
                self.nodes.push(Instance {
                    key: dep.target.clone(),
                    scope: Scope::new(),
                    children: Scope::new(),
                    parent,
                    canonical: id,
                    changed: context
                        .filter(|ctx| ctx.same.contains(name))
                        .map(|ctx| Rc::clone(&ctx.changed)),
                });
                Provider::Package(id)
            };
            (name.clone(), provider)
        })
        .collect()
    }
    fn canonical(&self, mut id: usize) -> usize {
        while self.nodes[id].canonical != id {
            id = self.nodes[id].canonical;
        }
        id
    }
    fn bind(&mut self, slots: &Scope, scope: &Scope) {
        for slot in slots.values() {
            let Provider::Package(id) = slot else {
                continue;
            };
            let free = &self.free[&self.nodes[*id].key];
            // The package's own dependency is replaced where the consumer provides that peer.
            let replaced =
                |edge: &str| scope.contains_key(edge) && free.get(edge).is_some_and(|r| r.own);
            self.nodes[*id].scope = free
                .iter()
                .filter(|(_, requirement)| requirement.active(replaced))
                .filter_map(|(n, _)| Some((n.clone(), scope.get(n)?.clone())))
                .collect();
        }
    }
    fn expand(&mut self) {
        let mut id = 0;
        while id < self.nodes.len() {
            let key = self.nodes[id].key.clone();
            let scope: Scope = self.nodes[id]
                .scope
                .iter()
                .map(|(n, p)| {
                    (
                        n.clone(),
                        match p {
                            Provider::Package(i) => Provider::Package(self.canonical(*i)),
                            p => p.clone(),
                        },
                    )
                })
                .collect();
            let changed = &self.nodes[id].changed;
            let lookup = (key.clone(), scope.clone(), changed.clone());
            let mut existing = self.memo.get(&lookup).copied();
            let mut ancestor = self.nodes[id].parent;
            while existing.is_none() {
                let Some(other) = ancestor else { break };
                let node = &self.nodes[other];
                if node.key == key
                    && node.scope.len() == scope.len()
                    && node
                        .scope
                        .iter()
                        .all(|(name, a)| match (a, scope.get(name)) {
                            (Provider::Package(a), Some(Provider::Package(b))) => {
                                self.nodes[*a].key == self.nodes[*b].key
                            }
                            (a, Some(b)) => a == b,
                            _ => false,
                        })
                {
                    existing = Some(other);
                    break;
                }
                ancestor = node.parent;
            }
            if let Some(other) = existing {
                self.nodes[id].canonical = other;
                id += 1;
                continue;
            }
            self.memo.insert(lookup, id);
            let node = &self.draft.nodes[&key];
            let context = self
                .history
                .as_ref()
                .zip(changed.as_deref())
                .map(|(history, changed)| history.children(&key, node, &scope, changed, self.free));
            // A consumer-provided peer takes precedence over the package's own
            // dependency of that name. Without a provider the ordinary edge wins.
            let children = self.slots(
                node.edges
                    .iter()
                    .filter(|(name, _)| !scope.contains_key(*name)),
                Some(id),
                context.as_ref(),
            );
            let mut visible = scope;
            visible.insert(
                manifest::selector(&node.package).0.into(),
                Provider::Package(id),
            );
            visible.extend(children.clone());
            self.bind(&children, &visible);
            self.nodes[id].children = children;
            id += 1;
        }
    }
    fn reference(&self, name: &str, provider: &Provider, keys: &BTreeMap<usize, String>) -> String {
        match provider {
            Provider::Link(s) => s.clone(),
            Provider::Locked(key) => reconcile::reference(name, key),
            Provider::Package(id) => reconcile::reference(name, &keys[&self.canonical(*id)]),
        }
    }
}

pub(super) fn project(draft: &mut Draft, build: &mut Build<'_, '_, '_>) -> Result<Value> {
    let mut auto = BTreeMap::<String, Dependencies>::new();
    let mut filled_optional = false;
    let free = loop {
        let (free, declarations) = requirements(draft);
        let mut added = false;
        if build.config.resolution.lock.auto_install_peers {
            for (id, deps) in &draft.roots {
                let previous = build.partition().scopes().get(id);
                let mut wants = BTreeMap::<&str, BTreeSet<usize>>::new();
                let mut optional = BTreeMap::<&str, BTreeSet<usize>>::new();
                let installed = auto.get(id);
                for (alias, dep) in deps
                    .iter()
                    .chain(installed.into_iter().flat_map(|d| d.iter()))
                {
                    let new_source = previous.and_then(|p| p.get(alias)).is_none_or(|key| {
                        graph::package_key(key) != graph::package_key(&dep.target)
                    });
                    for (name, reqs) in free.get(&dep.target).into_iter().flatten() {
                        if deps.contains_key(name)
                            || installed.is_some_and(|d| d.contains_key(name))
                            || (build.config.resolution.workspace_root_peers
                                && draft.roots["."].contains_key(name))
                        {
                            continue;
                        }
                        for &id in &reqs.declarations {
                            if !declarations[id].optional {
                                wants.entry(name).or_default().insert(id);
                            } else if declarations[id].install_optional
                                && !dep.locked
                                && (new_source
                                    || graph::dependency(
                                        &build.old["snapshots"][&declarations[id].owner],
                                        name,
                                    )
                                    .is_some())
                            {
                                optional.entry(name).or_default().insert(id);
                            }
                        }
                    }
                }
                let mut required_added = false;
                for (name, reqs, is_optional) in wants
                    .into_iter()
                    .map(|(n, r)| (n, r, false))
                    .chain(optional.into_iter().map(|(n, r)| (n, r, true)))
                {
                    if auto.get(id).is_some_and(|d| d.contains_key(name))
                        || is_optional
                            && (required_added
                                || !build.prefer_historical_peers
                                || !build.allow_optional_peers
                                || build
                                    .config
                                    .pnpm_version
                                    .as_ref()
                                    .is_some_and(|v| v.major() == 9 && v.minor() == 0))
                    {
                        continue;
                    }
                    let first = &declarations[*reqs.first().expect("peer declaration")];
                    let ranges: Vec<_> = reqs
                        .iter()
                        .map(|id| declarations[*id].range.as_str())
                        .collect();
                    let historical = if build.prefer_historical_peers {
                        build.historical_peer(&first.owner, name, &ranges, is_optional)?
                    } else {
                        None
                    };
                    let spec = if let Some(version) = historical {
                        version
                    } else if is_optional {
                        continue;
                    } else if ranges.iter().all(|r| *r == first.range) {
                        first.range.clone()
                    } else {
                        let mut range = first.range.parse::<node_semver::Range>().ok();
                        for r in ranges.iter().skip(1) {
                            range = range.and_then(|v| v.intersect(&r.parse().ok()?));
                        }
                        match range {
                            Some(v) => v.to_string(),
                            None => match build.covering_peer(name, &ranges)? {
                                Some(version) => version,
                                None => continue,
                            },
                        }
                    };
                    let Some(target) = build.auto_peer(id, &first.owner, name, &spec)? else {
                        continue;
                    };
                    auto.entry(id.clone()).or_default().insert(
                        name.to_owned(),
                        Dependency {
                            group: "dependencies",
                            target,
                            locked: false,
                        },
                    );
                    required_added |= !is_optional;
                    filled_optional |= is_optional;
                    added = true;
                }
            }
        }
        if !added {
            break free;
        }
        build.expand(draft)?;
    };
    let mut doc = std::mem::take(&mut draft.document);
    let mut instances = Instances {
        draft,
        free: &free,
        nodes: Vec::new(),
        memo: BTreeMap::new(),
        history: if build.project_interiors {
            None
        } else {
            lineage::History::new(build, &doc, &free)
        },
    };
    let mut root_scope = Scope::new();
    let mut root_context = None;
    let mut root_slots = BTreeMap::new();
    // Members inherit the root's peers, so the root binds first even when an
    // importer id such as `-app` sorts before `.`.
    let roots = draft.roots.iter().filter(|(id, _)| *id == ".");
    for (id, deps) in roots.chain(draft.roots.iter().filter(|(id, _)| *id != ".")) {
        let context = instances.history.as_ref().and_then(|h| {
            h.root(
                id,
                deps,
                if build.config.resolution.workspace_root_peers {
                    root_context.as_ref()
                } else {
                    None
                },
            )
        });
        let implicit = auto
            .get(id)
            .map(|deps| instances.slots(deps.iter(), None, None))
            .unwrap_or_default();
        let slots = instances.slots(deps.iter(), None, context.as_ref());
        let mut visible = if build.config.resolution.workspace_root_peers {
            root_scope.clone()
        } else {
            Scope::new()
        };
        visible.extend(implicit.clone());
        visible.extend(slots.clone());
        instances.bind(&implicit, &visible);
        instances.bind(&slots, &visible);
        if id == "." {
            root_scope = visible;
            root_context = context;
        }
        root_slots.insert(id, slots);
    }
    instances.expand();
    let limit = doc["settings"]["peersSuffixMaxLength"]
        .as_u64()
        .unwrap_or(1000);
    let bound = lineage::bound_peers(&instances);
    let mut nodes = instances
        .nodes
        .iter()
        .enumerate()
        .filter(|(id, n)| *id == n.canonical)
        .map(|(id, n)| {
            let node = &draft.nodes[&n.key];
            let canonical_scope = |scope: &Scope| {
                scope
                    .iter()
                    .map(|(k, p)| {
                        (
                            k.clone(),
                            match p {
                                Provider::Package(i) => Provider::Package(instances.canonical(*i)),
                                other => other.clone(),
                            },
                        )
                    })
                    .collect()
            };
            let mut providers: Scope = canonical_scope(&n.scope);
            if let Some(bound) = &bound {
                providers.retain(|name, _| bound[id].contains(name));
            }
            (
                id,
                Identity {
                    base: &node.package,
                    patch: node.patch.as_deref(),
                    providers,
                    children: canonical_scope(&n.children),
                    declared: &node.record["peerDependencies"],
                },
            )
        })
        .collect();
    let mut retained_roots: BTreeSet<_> = draft
        .roots
        .values()
        .flat_map(|deps| deps.values())
        .filter(|dep| dep.locked && !dep.target.starts_with("link:"))
        .map(|dep| dep.target.clone())
        .collect();
    for (id, instance) in instances.nodes.iter().enumerate() {
        if id == instance.canonical {
            for provider in instance.children.values() {
                if let Provider::Locked(key) = provider {
                    build.used_interior_history = true;
                    retained_roots.insert(key.clone());
                }
            }
        }
    }
    let retained = graph::closure_from_edges(&doc, &json!({}), &retained_roots)?.snapshots;
    let keys = identity::keys(&mut nodes, build.config, limit, &doc, &retained);
    // The draft contains unchanged incumbent bodies until projection emits an instance.
    let mut emitted = BTreeMap::new();
    let mut collisions = Vec::new();
    let rechecked_hook = build.seeded_history && build.reads_packages();
    for (id, slots) in root_slots {
        for (name, provider) in slots {
            let group = draft.roots[id][&name].group;
            let mut version = instances.reference(&name, &provider, &keys);
            if let Some(path) = version.strip_prefix("link:") {
                version = format!(
                    "link:{}",
                    crate::workspace_links::member_relative(id, path, build.config)
                );
            }
            doc["importers"][id][group][name]["version"] = version.into();
        }
    }
    for (&id, key) in &keys {
        let occurrence = &instances.nodes[id];
        let node = &draft.nodes[&occurrence.key];
        let mut body = node.body.clone();
        let reference = |name: &str, p: &Provider| instances.reference(name, p, &keys);
        // pnpm may have deduplicated a child's optional peer into a historical
        // instance even when the parent has no suffix for that peer. The printed
        // parent identity survives, together with its already-resolved children.
        if (node.reused || occurrence.key == *key && build.reads_packages())
            && (occurrence.key == *key || retained.contains(key))
            && !emitted.contains_key(key)
            && let Some(prior) = build.old["snapshots"].get(key)
            && doc["snapshots"].get(key).is_some()
            && (!rechecked_hook || node.matches_history(build.old, key, prior, build.config))
            && occurrence.scope.iter().all(|(name, provider)| {
                node.record["peerDependencies"].get(name).is_none()
                    || graph::dependency(prior, name).is_some_and(|(_, pin)| {
                        let wanted = reference(name, provider);
                        pin == wanted
                            || (build.config.resolution.lock.dedupe_peers
                                && graph::package_key(pin) == graph::package_key(&wanted)
                                && crate::peer::patch(pin) == crate::peer::patch(&wanted))
                    })
            })
        {
            if rechecked_hook {
                doc["packages"][&node.package] = node.record.clone();
            }
            emitted.entry(key).or_insert(key.as_str());
            continue;
        }
        for (name, p) in &occurrence.children {
            body[node.edges[name].group][name] = reference(name, p).into();
        }
        let own_name = manifest::selector(&node.package).0;
        if node.record["peerDependencies"].get(own_name).is_some()
            && !node.edges.contains_key(own_name)
        {
            let group = if node.record["peerDependenciesMeta"][own_name]["optional"] == true {
                "optionalDependencies"
            } else {
                "dependencies"
            };
            body[group][own_name] = reconcile::reference(own_name, key).into();
        }
        let mut transitive: BTreeSet<_> = nodes[&id]
            .providers
            .keys()
            .filter(|name| !free[&occurrence.key].contains_key(*name))
            .cloned()
            .collect();
        let free = &free[&occurrence.key];
        let replaced = |edge: &str| occurrence.scope.contains_key(edge);
        for (name, requirement) in free {
            if node.record["peerDependencies"].get(name).is_some() {
                if let Some(p) = occurrence.scope.get(name) {
                    let group = if node.record["peerDependenciesMeta"][name]["optional"] == true {
                        "optionalDependencies"
                    } else {
                        "dependencies"
                    };
                    body[group][name] = reference(name, p).into();
                }
            } else if requirement.active(replaced) {
                transitive.insert(name.clone());
            }
        }
        if !transitive.is_empty() {
            body["transitivePeerDependencies"] = json!(transitive);
        }
        if (emitted.contains_key(key) || retained.contains(key))
            && let Some(old) = doc["snapshots"].get(key)
            && *old != body
        {
            // Optional reachability is recomputed after all parents are known.
            if old
                .as_object()
                .into_iter()
                .flatten()
                .any(|(k, v)| k != "optional" && body.get(k) != Some(v))
                || body
                    .as_object()
                    .into_iter()
                    .flatten()
                    .any(|(k, v)| k != "optional" && old.get(k) != Some(v))
            {
                let left = emitted.get(key).copied().unwrap_or(key.as_str());
                collisions.push((key, left, occurrence.key.as_str(), &node.edges, body));
                continue;
            }
        }
        doc["packages"][&node.package] = node.record.clone();
        doc["snapshots"][key] = body;
        emitted.entry(key).or_insert(occurrence.key.as_str());
    }
    if build.config.resolution.dedupe_peer_dependents {
        // Several active callers can reach a single printable identity with
        // different optional peer bindings. Prefer a body that also satisfies
        // every edge of the current one. Check all callers against the final
        // body so the result does not depend on which caller we visited first.
        for (key, _, right, _, body) in &collisions {
            if !retained.contains(*key) && shares_children(&doc, key, body, &doc["snapshots"][*key])
            {
                doc["snapshots"][*key] = body.clone();
                emitted.insert(*key, right);
            }
        }
    }
    collisions.retain(|(key, _, _, _, body)| {
        if build.collapse_collisions
            || build.config.resolution.dedupe_peer_dependents
                && shares_children(&doc, key, &doc["snapshots"][*key], body)
        {
            return false;
        }
        if retained.contains(*key) {
            build.conflicting_history.insert((*key).clone());
        }
        true
    });
    build.optional_peer_collision = filled_optional && !collisions.is_empty();
    let mut conflict = None;
    for (key, left, right, ordinary, body) in collisions {
        let left = emitted.get(key).copied().unwrap_or(left);
        let prior = &doc["snapshots"][key];
        let error = reconcile::missing(
            &["snapshots", key],
            "different dependency bodies have the same peer identity",
        );
        let equalities = common_children((left, prior), (right, &body), ordinary, &build.demands);
        if equalities.is_empty() {
            build.blocked = Some(super::graph::Blocked::release(key));
            build.unrepairable_collision = true;
            return Err(error);
        }
        build.equalities.extend(equalities);
        conflict.get_or_insert(error);
    }
    if let Some(error) = conflict {
        return Err(error);
    }
    if !reconcile::published::records_fallback_peers(build.config) {
        let mut supplied = BTreeSet::new();
        for (id, occurrence) in instances.nodes.iter().enumerate() {
            if id == occurrence.canonical {
                let package = draft.nodes[&occurrence.key].package.as_str();
                supplied.extend(occurrence.scope.keys().map(|name| (package, name.as_str())));
            }
        }
        for key in &retained {
            let package = graph::package_key(key);
            if let Some(declared) =
                graph::section(&build.old["packages"][package], "peerDependencies")
            {
                supplied.extend(declared.keys().map(|name| (package, name.as_str())));
            }
        }
        for node in draft.nodes.values() {
            let Some(record) = doc
                .get_mut("packages")
                .and_then(|p| p.get_mut(&node.package))
            else {
                continue;
            };
            for name in node.edges.keys().filter(|name| {
                node.record["peerDependencies"].get(*name).is_some()
                    && !supplied.contains(&(node.package.as_str(), name.as_str()))
            }) {
                for group in ["peerDependencies", "peerDependenciesMeta"] {
                    if let Some(entries) = record.get_mut(group).and_then(Value::as_object_mut) {
                        entries.remove(name);
                        if entries.is_empty() {
                            record
                                .as_object_mut()
                                .expect("package record")
                                .remove(group);
                        }
                    }
                }
            }
        }
    }
    graph::prune(&mut doc, crate::DocumentKind::Project)?;
    graph::refresh_optional(&mut doc)?;
    Ok(doc)
}

/// A retained instance may supply an additional peer to a child. Prove that
/// every dependency and every peer binding requested by the incoming instance
/// is still present. Compare concrete child identities down to the first
/// shared node, including cycles and aliases.
fn shares_children(doc: &Value, key: &str, prior: &Value, incoming: &Value) -> bool {
    let snapshots = graph::section(doc, "snapshots");
    let mut next = Vec::new();
    let mut visited = BTreeSet::new();
    let compare = |package: &str, a: &Value, b: &Value, next: &mut Vec<(String, String)>| {
        if a.get("id") != b.get("id")
            || b["transitivePeerDependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|peer| {
                    !a["transitivePeerDependencies"]
                        .as_array()
                        .is_some_and(|names| names.contains(peer))
                })
        {
            return false;
        }
        for (group, name, target) in graph::snapshot_dependencies(b) {
            let Some((x, y)) = a[group][name].as_str().zip(target.as_str()) else {
                return false;
            };
            if x != y {
                if x.starts_with("link:") || y.starts_with("link:") {
                    return false;
                }
                next.push((
                    graph::snapshot_key(name, x, snapshots),
                    graph::snapshot_key(name, y, snapshots),
                ));
            }
        }
        let peers = &doc["packages"][package]["peerDependencies"];
        graph::snapshot_dependencies(a)
            .all(|(group, name, _)| b[group].get(name).is_some() || peers.get(name).is_some())
    };
    if !compare(graph::package_key(key), prior, incoming, &mut next) {
        return false;
    }
    while let Some(pair) = next.pop() {
        if visited.contains(&pair) {
            continue;
        }
        let (a, b) = &pair;
        let package = graph::package_key(a);
        if package != graph::package_key(b) || crate::peer::patch(a) != crate::peer::patch(b) {
            return false;
        }
        let Some((rich, requested)) =
            crate::peer::explicit_bindings(a).zip(crate::peer::explicit_bindings(b))
        else {
            return false;
        };
        if !requested
            .iter()
            .all(|(name, value)| rich.get(name) == Some(value))
        {
            return false;
        }
        let Some((rich, requested)) = doc["snapshots"].get(a).zip(doc["snapshots"].get(b)) else {
            return false;
        };
        if !compare(package, rich, requested, &mut next) {
            return false;
        }
        visited.insert(pair);
    }
    true
}

/// When an override or hook turns peers into ordinary dependencies, two old
/// instances can collapse to a single new identity. Its ordinary edge must
/// choose one version valid in both occurrences. Rebuild both scopes with
/// the same pin so all descendant peer bindings are checked again.
fn common_children(
    left: (&str, &Value),
    right: (&str, &Value),
    ordinary: &Dependencies,
    demands: &super::choices::Demands,
) -> Vec<(Vec<super::choices::Edge>, Vec<String>)> {
    use super::{RegistryPackage, choices::Owner};
    let mut equalities = Vec::new();
    for (group, alias, old) in graph::snapshot_dependencies(left.1) {
        if !ordinary.contains_key(alias) {
            continue;
        }
        let Some((a, b)) = old.as_str().zip(right.1[group][alias].as_str()) else {
            continue;
        };
        if a == b {
            continue;
        }
        let key = |pin: &str| {
            if RegistryPackage::from_key(pin).is_some() {
                pin.to_owned()
            } else {
                graph::snapshot_key(alias, pin, None)
            }
        };
        let a = key(a);
        let b = key(b);
        let Some(((p, v), (q, w))) =
            RegistryPackage::from_key(&a).zip(RegistryPackage::from_key(&b))
        else {
            continue;
        };
        if p != q || v == w {
            continue;
        }
        let mut edges = Vec::new();
        let mut domains = Vec::new();
        for (owner, version) in [(left.0, v), (right.0, w)] {
            let edge = (Owner::Snapshot(owner.to_owned()), alias.clone());
            if let Some(d) = demands
                .get(&edge)
                .filter(|d| d.package == p && d.selected == version)
            {
                domains.push(
                    d.versions
                        .iter()
                        .map(String::as_str)
                        .chain([version])
                        .collect::<BTreeSet<_>>(),
                );
                edges.push(edge);
            } else {
                domains.push(BTreeSet::from([version]));
            }
        }
        if edges.is_empty() {
            continue;
        }
        let mut versions: Vec<_> = domains[0]
            .intersection(&domains[1])
            .map(|v| v.to_string())
            .collect();
        // Try the two already witnessed resolutions before other eligible releases.
        versions.sort_by_key(|x| {
            if x == v {
                2
            } else if x == w {
                1
            } else {
                0
            }
        });
        equalities.push((edges, versions));
    }
    equalities
}
