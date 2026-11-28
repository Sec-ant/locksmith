//! Rebind locked snapshots and track generated records across provisional passes.
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    LinkedPeerEncoding, Value, model::graph, model::patch::hash_in_key, model::pin,
    model::pin::reference,
};

use super::{
    Merger, Origin, SavedRecord, bindings::Bindings, bound_reference, canonical_binding,
    canonical_providers, identity::context,
};

impl<'a> Merger<'a> {
    pub(crate) fn configure_context(
        &mut self,
        order: [usize; 4],
        dedupe_peers: bool,
        linked_peer_encoding: LinkedPeerEncoding,
    ) {
        self.preferred_sources = Some(order);
        self.dedupe_peers = dedupe_peers;
        self.linked_peer_encoding = Some(linked_peer_encoding);
    }
    pub(crate) fn recontextualize(
        &mut self,
        side: usize,
        key: &str,
        providers: &BTreeMap<String, String>,
        previous_providers: &BTreeMap<String, String>,
    ) -> Option<String> {
        let source = self.resolution_bindings(side, key, previous_providers)?;
        let origin = self
            .origins
            .get(key)
            .map(|origin| (origin.side, origin.original.clone()));
        let (side, key) = origin
            .as_ref()
            .map_or((side, key), |(side, key)| (*side, key.as_str()));
        self.recontextualize_bindings(side, key, source, providers, previous_providers)
    }

    pub(crate) fn recontextualize_source(
        &mut self,
        side: usize,
        key: &str,
        providers: &BTreeMap<String, String>,
        previous_providers: &BTreeMap<String, String>,
    ) -> Option<String> {
        let source = self.original_bindings(side, key, previous_providers)?;
        self.recontextualize_bindings(side, key, source, providers, previous_providers)
    }

    fn recontextualize_bindings(
        &mut self,
        side: usize,
        key: &str,
        source: Bindings,
        providers: &BTreeMap<String, String>,
        previous_providers: &BTreeMap<String, String>,
    ) -> Option<String> {
        let mut target = std::rc::Rc::unwrap_or_clone(source.peers);
        let providers = canonical_providers(providers);
        for (name, binding) in &mut target {
            if name != "patch_hash"
                && let Some(provider) = providers.get(name)
                && !(source.versions_only.contains(name) && pin::package_key(provider) == binding)
            {
                *binding = provider.clone();
            }
        }
        self.rebind(side, key, &target, previous_providers)
    }

    // Only the settled importer pass owns generated records. Intermediate
    // provider assignments may temporarily give the same identity another body.
    pub(crate) fn begin_pass(&mut self) {
        self.provisional = Some(Vec::new());
    }

    pub(crate) fn finish_pass(&mut self, settled: bool) {
        if !settled {
            self.rollback_to(0);
        }
        self.provisional.take().expect("active importer pass");
    }

    fn rollback_to(&mut self, checkpoint: usize) {
        let records = self.provisional.as_mut().expect("active rebound journal");
        for SavedRecord { key, body, origin } in records.drain(checkpoint..).rev() {
            if let Some(body) = body {
                self.generated.insert(key.clone(), body);
            } else {
                self.generated.remove(&key);
            }
            if let Some(origin) = origin {
                self.origins.insert(key, origin);
            } else {
                self.origins.remove(&key);
            }
        }
    }

    pub(crate) fn discard_generated(&mut self, key: &str) {
        self.generated.remove(key);
        self.origins.remove(key);
    }

    pub(crate) fn repatch(&mut self, side: usize, key: &str, hash: Option<&str>) -> Option<String> {
        if hash_in_key(key) == hash {
            return Some(key.into());
        }
        let mut bindings = self.binding_context(side, key, &BTreeMap::new())?;
        if let Some(hash) = hash {
            bindings.insert("patch_hash".into(), hash.into());
        } else {
            bindings.remove("patch_hash");
        }
        let package = pin::package_key(key);
        let suffix = &key[package.len()..];
        let suffix = if let Some(patched) = suffix.strip_prefix("(patch_hash=") {
            patched.split_once(')')?.1
        } else {
            suffix
        };
        let desired = match hash {
            Some(hash) => format!("{package}(patch_hash={hash}){suffix}"),
            None => format!("{package}{suffix}"),
        };
        let body = self
            .generated
            .get(key)
            .or_else(|| self.documents[side]?.get("snapshots")?.get(key))?
            .clone();
        let (side, original) = self
            .origins
            .get(key)
            .map_or((side, key.to_owned()), |origin| {
                (origin.side, origin.original.clone())
            });
        self.save_rebound(side, &original, &bindings, &desired, body)?;
        Some(desired)
    }

    pub(crate) fn record_context(
        &mut self,
        side: usize,
        original: &str,
        key: &str,
        bindings: BTreeMap<String, String>,
    ) {
        self.origins.insert(
            key.into(),
            Origin {
                side,
                original: original.into(),
                bindings: bindings.into(),
            },
        );
    }

    /// Propagate inherited peer bindings through a locked release's graph.
    /// A failed attempt leaves previously generated records intact.
    pub(crate) fn rebind(
        &mut self,
        side: usize,
        key: &str,
        target: &BTreeMap<String, String>,
        previous_providers: &BTreeMap<String, String>,
    ) -> Option<String> {
        let source = self.original_bindings(side, key, previous_providers)?;
        let verified = self
            .bindings
            .get(&(side, key.to_owned()))
            .is_some_and(Option::is_some);
        let encoding = self
            .linked_peer_encoding
            .unwrap_or(source.linked_peer_encoding);
        if verified && source.peers.as_ref() == target {
            let desired = if encoding != source.linked_peer_encoding
                && target.values().any(|value| value.starts_with("link:"))
            {
                source.identity(key, target, self.dedupe_peers, encoding)
            } else {
                key.into()
            };
            let body = self.documents[side]?.get("snapshots")?.get(key)?;
            self.save_rebound(side, key, target, &desired, body.clone())?;
            return Some(desired);
        }
        if source
            .peers
            .keys()
            .filter(|name| name.as_str() != "patch_hash")
            .ne(target.keys().filter(|name| name.as_str() != "patch_hash"))
        {
            return None;
        }
        let desired = source.identity(key, target, self.dedupe_peers, encoding);
        if !self.rebinding.insert(desired.clone()) {
            // The identity is determined before traversing the body. A back
            // edge can reference it while the owning frame finishes the node.
            return Some(desired);
        }
        let owns_journal = self.provisional.is_none();
        let checkpoint = self.provisional.get_or_insert_with(Vec::new).len();
        let result = self.rebind_body(side, key, &source, target, &desired, previous_providers);
        self.rebinding.remove(&desired);
        if result.is_none() {
            self.rollback_to(checkpoint);
        }
        if owns_journal {
            self.provisional = None;
        }
        result.map(|()| desired)
    }

    fn rebind_body(
        &mut self,
        side: usize,
        key: &str,
        source: &Bindings,
        target: &BTreeMap<String, String>,
        desired: &str,
        previous_providers: &BTreeMap<String, String>,
    ) -> Option<()> {
        let doc = self.documents[side]?;
        let original = doc.get("snapshots")?.get(key)?;
        let mut body = original.clone();
        let flat = source.flat || self.dedupe_peers;
        let source = &source.peers;
        let mut target = Cow::Borrowed(target);
        let mut child_scope = None;
        let package = pin::package_key(key);
        let own_peers = doc
            .get("packages")?
            .get(package)?
            .get("peerDependencies")
            .and_then(Value::as_object);
        let changed: BTreeSet<&str> = source
            .iter()
            .filter(|(name, value)| {
                name.as_str() != "patch_hash" && target.get(*name) != Some(*value)
            })
            .map(|(name, _)| name.as_str())
            .collect();
        for group in ["dependencies", "optionalDependencies"] {
            if let Some(deps) = body.get_mut(group).and_then(Value::as_object_mut) {
                for (name, value) in deps {
                    if own_peers.is_some_and(|m| m.contains_key(name)) {
                        if let Some(mut binding) = bound_reference(name, value.as_str()?, |name| {
                            target.get(name).map(String::as_str)
                        }) {
                            if flat && !binding.contains('(') && !binding.starts_with("link:") {
                                binding = self.concrete_provider(
                                    side,
                                    name,
                                    value.as_str()?,
                                    &binding,
                                    &target,
                                )?;
                                if target.get(name) != Some(&binding) {
                                    target.to_mut().insert(name.clone(), binding.clone());
                                }
                            }
                            *value = Value::String(binding);
                        }
                        continue;
                    }
                    let reference = value.as_str()?;
                    if reference.starts_with("link:") {
                        continue;
                    }
                    let child =
                        graph::snapshot_key(name, reference, graph::section(doc, "snapshots"));
                    if !affected_by(doc, &child, &changed) {
                        continue;
                    }
                    if child_scope.is_none() {
                        let mut scope = previous_providers.clone();
                        scope.extend(source.as_ref().clone());
                        for g in ["dependencies", "optionalDependencies"] {
                            for (n, v) in graph::section(original, g).into_iter().flatten() {
                                scope.insert(n.clone(), v.as_str()?.into());
                            }
                        }
                        child_scope = Some(scope);
                    }
                    let scope = child_scope.as_ref().expect("inherited provider scope");
                    let old = self.source_context(side, &child, scope)?;
                    let mut next = old.clone();
                    for (peer, binding) in &mut next {
                        if peer != "patch_hash"
                            && changed.contains(peer.as_str())
                            && source
                                .get(peer)
                                .is_some_and(|v| v == binding || v == pin::package_key(binding))
                            && let Some(new) = target.get(peer)
                        {
                            *binding = new.clone();
                        }
                    }
                    if next != old {
                        let child = self.recontextualize_source(side, &child, &next, scope)?;
                        *value = Value::String(pin::reference(name, &child));
                    }
                }
            }
        }
        Self::check_bindings([doc], package, &target, &body)?;
        self.save_rebound(side, key, &target, desired, body)
    }

    // Flattened identities specify a version, while dependency edges still
    // require a concrete snapshot. Preserve an existing context for that
    // release or rebind a locked replacement under the known provider scope.
    pub(crate) fn concrete_provider(
        &mut self,
        side: usize,
        name: &str,
        original: &str,
        binding: &str,
        providers: &BTreeMap<String, String>,
    ) -> Option<String> {
        let (actual, version) = canonical_binding(name, binding);
        let key = format!("{actual}@{version}");
        if self.generated.contains_key(&key)
            || self
                .documents
                .iter()
                .flatten()
                .any(|doc| doc.get("snapshots").and_then(|v| v.get(&key)).is_some())
        {
            return Some(binding.into());
        }
        if pin::package_key(original) == binding {
            return Some(original.into());
        }
        let (old_name, old_version) = canonical_binding(name, original);
        let old_key = format!("{old_name}@{old_version}");
        let mut scope = self.resolution_context(side, &old_key, providers)?;
        scope.remove("patch_hash");
        scope.extend(
            providers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
        let candidates = self.index.candidates().get(key.as_str())?.clone();
        let order = self
            .preferred_sources
            .unwrap_or([side, (side + 1) % 3, (side + 2) % 3, 3]);
        for source in order {
            for (candidate, _) in &candidates {
                if self.documents[source]
                    .and_then(|doc| doc.get("snapshots")?.get(*candidate))
                    .is_none()
                {
                    continue;
                }
                if let Some(resolved) =
                    self.recontextualize_source(source, candidate, &scope, &scope)
                {
                    return Some(reference(name, &resolved));
                }
            }
        }
        None
    }

    pub(crate) fn prefer_record(
        &self,
        old_side: usize,
        existing: &Value,
        side: usize,
        incoming: &Value,
    ) -> Option<bool> {
        if existing == incoming && (old_side == side || self.preferred_sources.is_none()) {
            return Some(false);
        }
        let order = self.preferred_sources?;
        if old_side == side {
            return (self.dedupe_peers && same_releases(existing, incoming)).then_some(false);
        }
        let rank = |side| {
            order
                .iter()
                .position(|source| *source == side)
                .expect("source rank")
        };
        Some(rank(side) < rank(old_side))
    }

    fn save_rebound(
        &mut self,
        side: usize,
        original: &str,
        bindings: &BTreeMap<String, String>,
        key: &str,
        body: Value,
    ) -> Option<()> {
        if let Some(existing) = self.generated.get(key) {
            if existing == &body && self.preferred_sources.is_none() {
                return Some(());
            }
            let old_side = self.origin_side(key)?;
            let replace = self
                .prefer_record(old_side, existing, side, &body)
                .or_else(|| {
                    self.preferred_sources?;
                    cycle_record(key, existing, &body)
                })?;
            if !replace {
                return Some(());
            }
        }
        let previous = self.generated.insert(key.into(), body);
        let origin = self.origins.insert(
            key.into(),
            Origin {
                side,
                original: original.into(),
                bindings: bindings.clone().into(),
            },
        );
        if let Some(records) = &mut self.provisional {
            records.push(SavedRecord {
                key: key.into(),
                body: previous,
                origin,
            });
        }
        Some(())
    }

    pub(super) fn check_bindings<'d>(
        documents: impl IntoIterator<Item = &'d Value>,
        package: &str,
        bindings: &BTreeMap<String, String>,
        snapshot: &Value,
    ) -> Option<()> {
        for doc in documents {
            let Some(peers) = doc
                .get("packages")
                .and_then(|p| p.get(package))
                .and_then(|p| p.get("peerDependencies"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            for name in peers.keys() {
                let actual = snapshot
                    .get("dependencies")
                    .and_then(|d| d.get(name))
                    .or_else(|| {
                        snapshot
                            .get("optionalDependencies")
                            .and_then(|d| d.get(name))
                    })
                    .and_then(Value::as_str);
                let Some(binding) = bound_reference(name, actual.unwrap_or_default(), |name| {
                    bindings.get(name).map(String::as_str)
                }) else {
                    continue;
                };
                let actual = actual?;
                if actual != binding {
                    return None;
                }
            }
        }
        Some(())
    }
}

// Two sides of a peer cycle print each other's release without their nested
// providers. A fully spelled sibling on the parent can still distinguish the
// competing child records. Every difference must identify the same winner.
fn cycle_record(key: &str, existing: &Value, candidate: &Value) -> Option<bool> {
    if !same_releases(existing, candidate)
        || existing.get("transitivePeerDependencies") != candidate.get("transitivePeerDependencies")
    {
        return None;
    }
    let parent = context(key)?;
    let (parent_name, parent_version) = pin::package_key(key).rsplit_once('@')?;
    let mut selected = None;
    for group in ["dependencies", "optionalDependencies"] {
        for (name, old) in graph::section(existing, group).into_iter().flatten() {
            let new = candidate.get(group)?.get(name)?;
            if new == old {
                continue;
            }
            let (child_name, old) = canonical_binding(name, old.as_str()?);
            let (_, new) = canonical_binding(name, new.as_str()?);
            if parent.get(child_name).copied() != Some(pin::package_key(old)) {
                return None;
            }
            let before = context(old)?;
            let after = context(new)?;
            if before.get(parent_name).copied() != Some(parent_version)
                || after.get(parent_name).copied() != Some(parent_version)
                || before.keys().ne(after.keys())
            {
                return None;
            }
            for (peer, old_value) in before {
                let new_value = after[peer];
                if old_value == new_value {
                    continue;
                }
                let wanted = *parent.get(peer)?;
                let replace = if new_value == wanted {
                    true
                } else if old_value == wanted {
                    false
                } else {
                    return None;
                };
                if selected
                    .replace(replace)
                    .is_some_and(|last| last != replace)
                {
                    return None;
                }
            }
        }
    }
    selected
}

pub(crate) fn same_releases(a: &Value, b: &Value) -> bool {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return false;
    };
    // This list also records unresolved inherited peers. Distinct nested
    // contexts can therefore have different lists while sharing a deduplicated
    // identity. Keep the selected record together with its own derived list.
    let fields = |body: &serde_json::Map<String, Value>| {
        body.len() - usize::from(body.contains_key("transitivePeerDependencies"))
    };
    fields(a) == fields(b)
        && a.iter()
            .filter(|(field, _)| field.as_str() != "transitivePeerDependencies")
            .all(|(field, value)| {
                let Some(other) = b.get(field) else {
                    return false;
                };
                if !matches!(field.as_str(), "dependencies" | "optionalDependencies") {
                    return value == other;
                }
                let (Some(value), Some(other)) = (value.as_object(), other.as_object()) else {
                    return false;
                };
                value.len() == other.len()
                    && value.iter().all(|(name, reference)| {
                        let (Some(reference), Some(other)) =
                            (reference.as_str(), other.get(name).and_then(Value::as_str))
                        else {
                            return false;
                        };
                        pin::package_key(reference) == pin::package_key(other)
                            && hash_in_key(reference) == hash_in_key(other)
                    })
            })
}

fn affected_by(doc: &Value, key: &str, changed: &BTreeSet<&str>) -> bool {
    if let Some(bindings) = context(key)
        && bindings.keys().any(|name| changed.contains(name))
    {
        return true;
    }
    let snapshot = doc.get("snapshots").and_then(|v| v.get(key));
    let affected = |name: &str| {
        changed.contains(name)
            || ["dependencies", "optionalDependencies"]
                .into_iter()
                .any(|group| {
                    snapshot
                        .and_then(|v| v.get(group))
                        .and_then(|v| v.get(name))
                        .and_then(Value::as_str)
                        .and_then(|r| pin::package_key(r).rsplit_once('@'))
                        .is_some_and(|(actual, _)| changed.contains(actual))
                })
    };
    doc.get("packages")
        .and_then(|v| v.get(pin::package_key(key)))
        .and_then(|v| v.get("peerDependencies"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .any(|(name, _)| affected(name))
        || snapshot
            .and_then(|v| v.get("transitivePeerDependencies"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verified_linked_bindings_use_the_configured_encoding() {
        let doc = json!({
            "importers": {".": {"dependencies": {
                "local": {"version": "link:con"},
                "lib": {"version": "1.0.0(local@con+)"}
            }}, "con": {}},
            "packages": {"lib@1.0.0": {"peerDependencies": {"local": "*"}}},
            "snapshots": {"lib@1.0.0(local@con+)": {"dependencies": {"local": "link:con"}}}
        });
        let mut merger = Merger::new([Some(&doc), None, None]);
        merger.configure_context([0, 1, 2, 3], false, LinkedPeerEncoding::Path);
        let bindings = BTreeMap::from([("local".into(), "link:con".into())]);
        let key = merger.rebind(0, "lib@1.0.0(local@con+)", &bindings, &bindings);
        assert_eq!(key.as_deref(), Some("lib@1.0.0(local@con)"));
        let (_, body) = merger.rebound_snapshot("lib@1.0.0(local@con)").unwrap();
        assert_eq!(body["dependencies"]["local"], "link:con");
    }

    #[test]
    fn conflicting_rebind_preserves_only_previously_generated_snapshots() {
        let doc = json!({
            "packages": {
                "a@1.0.0": {"peerDependencies": {"x": "*"}},
                "parent@1.0.0": {"peerDependencies": {"x": "*"}},
                "z@1.0.0": {"peerDependencies": {"x": "*"}},
                "leaf@1.0.0": {}, "leaf@2.0.0": {}, "x@1.0.0": {}, "x@2.0.0": {}
            },
            "snapshots": {
                "a@1.0.0(x@1.0.0)": {"dependencies": {"x": "1.0.0"}},
                "parent@1.0.0(x@1.0.0)": {"dependencies": {
                    "a": "1.0.0(x@1.0.0)", "x": "1.0.0", "z": "1.0.0(x@1.0.0)"
                }},
                "z@1.0.0(x@1.0.0)": {"dependencies": {"leaf": "1.0.0", "x": "1.0.0"}},
                "z@1.0.0(x@2.0.0)": {"dependencies": {"leaf": "2.0.0", "x": "2.0.0"}},
                "leaf@1.0.0": {}, "leaf@2.0.0": {}, "x@1.0.0": {}, "x@2.0.0": {}
            }
        });
        let mut merger = Merger::new([Some(&doc), None, None]);
        let old = BTreeMap::from([("x".into(), "1.0.0".into())]);
        let new = BTreeMap::from([("x".into(), "2.0.0".into())]);
        let existing = "z@1.0.0(x@2.0.0)";
        assert_eq!(
            merger.rebind(0, existing, &new, &new).as_deref(),
            Some(existing)
        );
        assert!(
            merger
                .rebind(0, "parent@1.0.0(x@1.0.0)", &new, &old)
                .is_none()
        );
        assert!(merger.rebound_snapshot("a@1.0.0(x@2.0.0)").is_none());
        let (_, body) = merger.rebound_snapshot(existing).unwrap();
        assert_eq!(body["dependencies"]["leaf"], "2.0.0");
    }
}
