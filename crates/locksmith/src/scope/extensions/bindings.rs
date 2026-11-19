//! Propagate only the external peer bindings changed by an edited dependency.
use std::collections::{BTreeMap, BTreeSet};

use super::{EDGES, Instance, Projector, SnapshotEdit, dependency, edges, transitive};
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
        a == b
            || self.config.resolution.lock.dedupe_peers
                && graph::package_key(a) == graph::package_key(b)
                && (!a.contains('(') || !b.contains('('))
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
        edit: &SnapshotEdit,
    ) -> Result<(), LockfileError> {
        let [base, donor] = self.docs;
        let previous = &base["snapshots"][old];
        let installed = &donor["snapshots"][next];
        for provider in &edit.touched {
            if transitive(previous).any(|n| n == provider)
                != transitive(installed).any(|n| n == provider)
            {
                return Err(missing(
                    &["snapshots", old, provider],
                    "edited dependency changes the source of an inherited peer",
                ));
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
                    && self.interface(1, &witness)?.get(provider) == Some(&binding)
                {
                    continue;
                }
                return Err(missing(
                    &["snapshots", old, sibling, provider],
                    "edited local provider requires a different sibling peer binding",
                ));
            }
        }
        Ok(())
    }

    fn combine(
        &self,
        parent: &str,
        name: &str,
        total: &mut Binding,
        next: &Binding,
    ) -> Result<(), LockfileError> {
        match (&*total, next) {
            (Binding::Absent, _) | (Binding::Unbound, Binding::Bound(_)) => *total = next.clone(),
            (Binding::Bound(a), Binding::Bound(b)) if !self.same_binding(a, b) => {
                return Err(missing(
                    &["snapshots", parent, name],
                    "dependencies require competing peer bindings",
                ));
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
    ) -> Result<Instance, LockfileError> {
        let base = self.docs[0];
        let own = &base["packages"][graph::package_key(old)]["peerDependencies"];
        let mut inherited: BTreeSet<_> = transitive(body).map(str::to_owned).collect();
        let mut changes = Edits::new();
        let mut previous = None;
        for (name, children) in edits {
            let mut after = Binding::Absent;
            for contribution in children.values() {
                self.combine(old, &name, &mut after, contribution)?;
            }
            let local = EDGES
                .iter()
                .find_map(|g| body[*g].get(&name).and_then(Value::as_str));
            if let Some(provider) = local {
                if let Binding::Bound(bound) = &after
                    && !self.same_binding(provider, bound)
                {
                    return Err(missing(
                        &["snapshots", old, &name],
                        "extension dependency uses a different local peer provider",
                    ));
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
                        self.combine(old, &name, &mut retained, &bound)?;
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
                self.combine(old, &name, &mut after, &retained)?;
                for key in opaque {
                    let bound = self
                        .bindings(0, &key)?
                        .remove(&name)
                        .map_or(Binding::Unbound, Binding::Bound);
                    self.combine(old, &name, &mut after, &bound)?;
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
                if !verified(next) {
                    return Err(missing(
                        &["snapshots", old],
                        "installed identity does not verify the projected peer bindings",
                    ));
                }
                key = next.into();
            }
        }
        Ok(Instance {
            key,
            peers: changes,
        })
    }
}
