//! Propagate only the external peer bindings changed by an edited dependency.
use std::collections::{BTreeMap, BTreeSet};

use super::{
    EDGES, Instance, ProjectionError, Projector, SnapshotEdit, dependency, edges, transitive,
};
use crate::{LockfileError, Value, graph, peer, reconcile::missing};

type Bindings = BTreeMap<String, String>;
/// A missing optional peer still appears in transitivePeerDependencies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum Binding {
    #[default]
    Absent,
    Unbound,
    Bound(String),
}
pub(super) type Interface = BTreeMap<String, Binding>;
pub(super) type Edits = BTreeMap<String, (Binding, Binding)>;
// For each external peer, only children whose contribution actually changed.
pub(super) type Contributions = BTreeMap<String, BTreeMap<String, Binding>>;

impl Projector<'_> {
    pub(super) fn same_binding(&self, a: &str, b: &str) -> bool {
        same_binding(a, b, self.config.resolution.lock.dedupe_peers)
    }

    fn bindings(&mut self, side: usize, key: &str) -> Result<Bindings, LockfileError> {
        if let Some(map) = peer::explicit_bindings(key) {
            return Ok(map.into_iter().map(|(k, v)| (k.into(), v.into())).collect());
        }
        self.peers
            .binding_context(side, key, &BTreeMap::new())
            .ok_or_else(|| missing(&["snapshots", key], "peer bindings cannot be verified"))
    }

    pub(super) fn interface(&mut self, side: usize, key: &str) -> Result<Interface, LockfileError> {
        let doc = self.docs[side];
        let mut exposed: Interface = self
            .bindings(side, key)?
            .into_iter()
            .filter(|(n, _)| n != "patch_hash")
            .map(|(n, v)| (n, Binding::Bound(v)))
            .collect();
        for n in transitive(&doc["snapshots"][key]).chain(
            graph::section(
                &doc["packages"][graph::package_key(key)],
                "peerDependencies",
            )
            .into_iter()
            .flatten()
            .map(|(n, _)| n.as_str()),
        ) {
            exposed.entry(n.into()).or_insert(Binding::Unbound);
        }
        Ok(exposed)
    }

    /// A dependency edited by an extension can itself provide a peer for a
    /// sibling. Only retain that sibling when its binding still fits the scope.
    pub(super) fn verify_local_providers(
        &mut self,
        old: &str,
        next: &str,
        edit: &mut SnapshotEdit,
    ) -> Result<(), ProjectionError> {
        let [base, donor] = self.docs;
        let previous = &base["snapshots"][old];
        let installed = &donor["snapshots"][next];
        let mut replacements = Vec::new();
        for provider in &edit.touched {
            if transitive(previous).any(|n| n == provider)
                != transitive(installed).any(|n| n == provider)
                && !edit.declared.contains_key(provider)
            {
                let before = self.interface(0, old)?.remove(provider).unwrap_or_default();
                let after = self
                    .interface(1, next)?
                    .remove(provider)
                    .unwrap_or_default();
                edit.declared.insert(provider.clone(), (before, after));
            }
            for (_, sibling, pin) in
                edges(previous).filter(|(_, n, _)| *n != provider && !edit.touched.contains(*n))
            {
                let pin = pin.as_str().ok_or_else(|| {
                    missing(&["snapshots", old, sibling], "invalid dependency reference")
                })?;
                let key = graph::snapshot_key(sibling, pin, graph::section(base, "snapshots"));
                if base["packages"][graph::package_key(&key)]["peerDependencies"]
                    .get(provider)
                    .is_none()
                    && !transitive(&base["snapshots"][&key]).any(|n| n == provider)
                {
                    continue;
                }
                let witnessed = dependency(installed, sibling).map(|(_, r)| {
                    graph::snapshot_key(sibling, r, graph::section(donor, "snapshots"))
                });
                let retained = edit.peers.get(provider).and_then(|p| p.get(sibling));
                let binding = match retained {
                    Some(bound) => bound.clone(),
                    None => self
                        .interface(0, &key)?
                        .remove(provider)
                        .unwrap_or_default(),
                };
                if let (Some((_, supplied)), Binding::Bound(bound)) =
                    (dependency(&edit.body, provider), &binding)
                    && self.same_binding(supplied, bound)
                {
                    continue;
                }
                if let Some(witness) = witnessed
                    && graph::package_key(&key) == graph::package_key(&witness)
                    && peer::patch(&key) == peer::patch(&witness)
                {
                    let installed_binding = self
                        .interface(1, &witness)?
                        .remove(provider)
                        .unwrap_or_default();
                    if installed_binding == binding && dependency(&edit.body, provider).is_none() {
                        continue;
                    }
                    // Keep the sibling's ordinary edges and other peer bindings.
                    // pnpm's installed instance witnesses the changed local provider.
                    if !self.affected.contains(&key)
                        && let Some((_, supplied)) = dependency(&edit.body, provider)
                        && let Binding::Bound(pin) = &installed_binding
                        && self.same_binding(supplied, pin)
                    {
                        replacements.push((
                            sibling.clone(),
                            key,
                            provider.clone(),
                            binding,
                            installed_binding,
                        ));
                        continue;
                    }
                }
                return Err(ProjectionError::MixedPeers);
            }
        }
        if !replacements.is_empty() {
            let mut previous_scope = self.bindings(0, old)?;
            previous_scope.extend(
                edges(previous).filter_map(|(_, n, p)| Some((n.clone(), p.as_str()?.into()))),
            );
            let mut targets = BTreeMap::<_, Bindings>::new();
            for (sibling, key, provider, before, after) in replacements {
                if let Binding::Bound(pin) = &after {
                    targets
                        .entry((sibling.clone(), key))
                        .or_default()
                        .insert(provider.clone(), pin.clone());
                }
                edit.peer_change(&sibling, &provider, &before, &after);
            }
            for ((sibling, key), replacements) in targets {
                let err = || {
                    missing(
                        &["snapshots", old, &sibling],
                        "cannot verify the sibling instance for its local providers",
                    )
                };
                let mut desired = self
                    .peers
                    .binding_context(0, &key, &previous_scope)
                    .ok_or_else(err)?;
                desired.extend(replacements);
                let rebound = self
                    .peers
                    .rebind(0, &key, &desired, &previous_scope)
                    .ok_or_else(err)?;
                let (group, _) = dependency(&edit.body, &sibling).expect("retained sibling");
                edit.body[group][&sibling] = crate::reconcile::reference(&sibling, &rebound).into();
            }
        }
        Ok(())
    }

    fn combine(&self, total: &mut Binding, next: &Binding) -> Result<(), ProjectionError> {
        match (&*total, next) {
            (Binding::Absent, _) | (Binding::Unbound, Binding::Bound(_)) => *total = next.clone(),
            (Binding::Bound(a), Binding::Bound(b)) if !self.same_binding(a, b) => {
                return Err(ProjectionError::MixedPeers);
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn parent(
        &mut self,
        old: &str,
        next: &str,
        body: &mut Value,
        edits: Contributions,
        mut changes: Edits,
    ) -> Result<Instance, ProjectionError> {
        let base = self.docs[0];
        let own = &self.docs[1]["packages"][graph::package_key(next)]["peerDependencies"];
        let mut inherited: BTreeSet<_> = transitive(body).map(str::to_owned).collect();
        for name in changes.keys() {
            if transitive(&self.docs[1]["snapshots"][next]).any(|n| n == name) {
                inherited.insert(name.clone());
            } else {
                inherited.remove(name);
            }
        }
        let mut previous = None;
        for (name, children) in edits {
            let mut after = Binding::Absent;
            for contribution in children.values() {
                self.combine(&mut after, contribution)?;
            }
            let local = EDGES
                .iter()
                .find_map(|g| body[*g].get(&name).and_then(Value::as_str));
            if let Some(provider) = local {
                if let Binding::Bound(bound) = &after
                    && !self.same_binding(provider, bound)
                {
                    return Err(ProjectionError::MixedPeers);
                }
                continue;
            }
            if name
                == graph::package_key(old)
                    .rsplit_once('@')
                    .map_or("", |(n, _)| n)
                || own.get(&name).is_some()
            {
                continue;
            }
            let was = inherited.contains(&name);
            if was {
                let siblings = edges(&base["snapshots"][old])
                    .filter(|(_, child, _)| !children.contains_key(*child))
                    .filter_map(|(_, child, pin)| {
                        let key = graph::snapshot_key(
                            child,
                            pin.as_str()?,
                            graph::section(base, "snapshots"),
                        );
                        let exposed = transitive(&base["snapshots"][&key]).any(|n| n == name)
                            || base["packages"][graph::package_key(&key)]["peerDependencies"]
                                .get(&name)
                                .is_some();
                        exposed.then_some(key)
                    });
                let mut opaque = Vec::new();
                let mut retained = Binding::Absent;
                for key in siblings {
                    if let Some(pins) = peer::explicit_bindings(&key) {
                        let bound = pins
                            .get(name.as_str())
                            .map_or(Binding::Unbound, |v| Binding::Bound((*v).into()));
                        self.combine(&mut retained, &bound)?;
                    } else {
                        opaque.push(key);
                    }
                }
                // A retained sibling that still supplies the requested provider
                // also witnesses the existing binding of an opaque parent.
                if let (Binding::Bound(a), Binding::Bound(b)) = (&after, &retained)
                    && self.same_binding(a, b)
                {
                    continue;
                }
                self.combine(&mut after, &retained)?;
                for key in opaque {
                    let bound = self
                        .bindings(0, &key)?
                        .remove(&name)
                        .map_or(Binding::Unbound, Binding::Bound);
                    self.combine(&mut after, &bound)?;
                }
            }
            let before = if was {
                if previous.is_none() {
                    previous = Some(self.bindings(0, old)?);
                }
                previous
                    .as_ref()
                    .unwrap()
                    .get(&name)
                    .cloned()
                    .map_or(Binding::Unbound, Binding::Bound)
            } else {
                Binding::Absent
            };
            if before == after {
                continue;
            }
            if after != Binding::Absent {
                inherited.insert(name.clone());
            } else {
                inherited.remove(&name);
            }
            changes.insert(name, (before, after));
        }
        let mut key = old.to_owned();
        if !changes.is_empty() {
            if !inherited.is_empty() {
                body["transitivePeerDependencies"] =
                    inherited.into_iter().collect::<Vec<_>>().into();
            } else {
                body.as_object_mut()
                    .unwrap()
                    .remove("transitivePeerDependencies");
            }
            let mut pins = match previous {
                Some(pins) => pins,
                None => self.bindings(0, old)?,
            };
            for (n, (_, new)) in &changes {
                if let Binding::Bound(pin) = new {
                    pins.insert(n.clone(), pin.clone());
                } else {
                    pins.remove(n);
                }
            }
            let verified = |k| {
                peer::matches_bindings(
                    k,
                    &pins,
                    self.config.resolution.lock.dedupe_peers,
                    self.config.linked_peer_encoding,
                )
            };
            if !verified(old) {
                key = if verified(next) {
                    next.to_owned()
                } else {
                    let original = self.bindings(0, old)?;
                    let installed = self.bindings(1, next)?;
                    peer::projected_identity(
                        [old, next],
                        [&original, &installed],
                        &pins,
                        self.config,
                        self.docs[1],
                    )
                    .ok_or_else(|| {
                        missing(
                            &["snapshots", old],
                            "installed identity does not verify the projected peer bindings",
                        )
                    })?
                };
            }
        }
        Ok(Instance {
            key,
            peers: changes,
        })
    }
}

pub(super) struct Root {
    pub(super) id: String,
    pub(super) group: &'static str,
    pub(super) name: String,
    pub(super) instance: Instance,
}

impl super::Projection {
    /// Choose peer providers after catalog pins, direct overrides and manifest
    /// edits have selected the actual importer scope.
    pub(in crate::scope) fn bind_roots(
        self,
        result: &mut crate::Lockfile,
        baseline: &crate::Lockfile,
        candidate: &crate::Lockfile,
        config: &crate::WorkspaceConfig,
    ) -> Result<(), LockfileError> {
        if self.bindings.is_empty() {
            return Ok(());
        }
        let doc = result.project().unwrap();
        let donor = candidate.project().unwrap();
        let mut merger = None;
        let mut edits = Vec::new();
        let mut scopes = BTreeMap::new();
        for root in self.bindings {
            let Root {
                id,
                group,
                name,
                instance,
            } = root;
            let Some(pin) = doc["importers"][&id][group][&name]["version"].as_str() else {
                continue;
            };
            if graph::snapshot_key(&name, pin, graph::section(doc, "snapshots")) != instance.key {
                continue;
            }
            let scope = scopes.entry(id.clone()).or_insert_with(|| {
                peer::importer_providers(
                    &doc["importers"],
                    &id,
                    config.resolution.workspace_root_peers,
                )
            });
            let scope = peer::canonical_providers(scope);
            let mut replacements = BTreeMap::new();
            for (peer, (_, new)) in instance.peers {
                if new == Binding::Unbound
                    && let Some(target) = scope.get(&peer)
                    && let Some(range) =
                        doc["packages"][graph::package_key(&instance.key)]["peerDependencies"]
                            [&peer]
                            .as_str()
                    && crate::reconcile::accepts(&peer, range, target, doc)
                {
                    return Err(missing(
                        &["importers", &id, group, &name, &peer],
                        "installed instance lacks the declared peer supplied by this consumer",
                    ));
                }
                if let Binding::Bound(value) = new
                    && let Some(target) = scope.get(&peer)
                    && !same_binding(&value, target, config.resolution.lock.dedupe_peers)
                {
                    replacements.insert(peer, target.clone());
                }
            }
            if replacements.is_empty() {
                continue;
            }
            let peers = merger.get_or_insert_with(|| {
                let mut m = peer::Merger::new([Some(doc), Some(donor), baseline.project()]);
                m.configure_context(
                    [0, 1, 2, 3],
                    config.resolution.lock.dedupe_peers,
                    config.linked_peer_encoding,
                );
                m
            });
            let original = peer::importer_providers(
                &donor["importers"],
                &id,
                config.resolution.workspace_root_peers,
            );
            let error = || {
                missing(
                    &["importers", &id, group, &name],
                    "cannot verify the instance for the selected peer providers",
                )
            };
            let mut target = peers
                .binding_context(0, &instance.key, &original)
                .ok_or_else(error)?;
            target.extend(replacements);
            let key = peers
                .rebind(0, &instance.key, &target, &original)
                .ok_or_else(error)?;
            edits.push((
                id,
                group,
                name.clone(),
                crate::reconcile::reference(&name, &key),
            ));
        }
        if let Some(peers) = merger {
            let generated = peers.generated;
            let output = std::sync::Arc::make_mut(
                result
                    .documents
                    .get_mut(&crate::DocumentKind::Project)
                    .unwrap(),
            );
            insert_rebound(output, generated)?;
            for (id, group, name, pin) in edits {
                output["importers"][id][group][name]["version"] = pin.into();
            }
            graph::prune_scoped(
                output,
                crate::DocumentKind::Project,
                baseline.project().unwrap(),
                Some(config),
            )?;
        }
        Ok(())
    }
}
pub(super) fn insert_rebound(
    doc: &mut Value,
    generated: BTreeMap<String, Value>,
) -> Result<(), LockfileError> {
    for (key, body) in generated {
        if let Some(existing) = doc["snapshots"].get(&key) {
            // Optionality is recomputed from the final roots.
            if existing != &body
                && [existing, &body].into_iter().any(|r| {
                    r.as_object().is_none_or(|m| {
                        m.keys()
                            .any(|k| k != "optional" && existing.get(k) != body.get(k))
                    })
                })
            {
                return Err(missing(
                    &["snapshots", &key],
                    "peer rebinding requires different contents for the same instance",
                ));
            }
        } else {
            doc["snapshots"][key] = body;
        }
    }
    Ok(())
}

fn same_binding(a: &str, b: &str, flat: bool) -> bool {
    a == b
        || flat
            && graph::package_key(a) == graph::package_key(b)
            && (!a.contains('(') || !b.contains('('))
}
