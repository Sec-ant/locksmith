//! Recover and verify the peer bindings of locked snapshots.
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use crate::{Value, checksum::HashStyle, graph, patch::hash_in_key, pin};

use super::{
    BindingCandidates, Merger, Origin,
    bindings::{self, Bindings, ConstructedBindings, analyze_bindings, bindings},
    canonical_binding, canonical_providers,
    identity::{self, context, context_tokens},
};

impl<'a> Merger<'a> {
    pub(crate) fn context_identity(
        &mut self,
        side: usize,
        key: &str,
        target: &BTreeMap<String, String>,
        previous: &BTreeMap<String, String>,
    ) -> Option<String> {
        let source = self.resolution_bindings(side, key, previous)?;
        Some(
            source.identity(
                key,
                target,
                self.dedupe_peers,
                self.linked_peer_encoding
                    .unwrap_or(source.linked_peer_encoding),
            ),
        )
    }

    /// A named snapshot can witness a peer cycle even when its shorthand
    /// cannot be expanded into a unique set of nested bindings.
    pub(crate) fn witnesses_scope(
        &self,
        side: usize,
        original: (usize, &str),
        candidate: &str,
        providers: &BTreeMap<String, String>,
    ) -> bool {
        let (original_side, original) = original;
        let Some(before) = context(original) else {
            return false;
        };
        let Some(after) = context(candidate) else {
            return false;
        };
        if before.keys().ne(after.keys()) {
            return false;
        }
        let Some(doc) = self.documents[side] else {
            return false;
        };
        let record = &doc["snapshots"][candidate];
        if self
            .generated
            .get(candidate)
            .is_some_and(|generated| generated != record)
        {
            return false;
        }
        // Only an existing instance may keep a shortened token. Its concrete
        // direct peer edges must agree with every visible declared provider.
        for (name, value) in &before {
            let observed = after[name];
            if let Some(provider) = providers.get(*name) {
                if !identity::token_matches(name, provider, name, observed) {
                    return false;
                }
            } else if observed != *value {
                return false;
            }
        }
        for (name, _) in graph::section(
            &doc["packages"][pin::package_key(candidate)],
            "peerDependencies",
        )
        .into_iter()
        .flatten()
        {
            let edge = |record: &'a Value| {
                ["dependencies", "optionalDependencies"]
                    .into_iter()
                    .find_map(|group| record.get(group)?.get(name)?.as_str())
            };
            let original_doc = self.documents[original_side].expect("locked source");
            let wanted = providers
                .get(name)
                .map(String::as_str)
                .or_else(|| edge(&original_doc["snapshots"][original]));
            if edge(record) != wanted {
                return false;
            }
        }
        true
    }

    pub(crate) fn binding_context(
        &mut self,
        side: usize,
        key: &str,
        previous_providers: &BTreeMap<String, String>,
    ) -> Option<BTreeMap<String, String>> {
        self.binding_context_with(side, key, || Cow::Borrowed(previous_providers))
    }

    pub(crate) fn binding_context_with<'p>(
        &mut self,
        side: usize,
        key: &str,
        previous_providers: impl FnOnce() -> Cow<'p, BTreeMap<String, String>>,
    ) -> Option<BTreeMap<String, String>> {
        if let Some(Origin { bindings, .. }) = self.origins.get(key) {
            return Some(bindings.as_ref().clone());
        }
        self.source_bindings_with(side, key, previous_providers)
            .map(|context| context.peers.as_ref().clone())
    }

    fn source_bindings_with<'p>(
        &mut self,
        side: usize,
        key: &str,
        previous_providers: impl FnOnce() -> Cow<'p, BTreeMap<String, String>>,
    ) -> Option<&Bindings> {
        let cached = self.bindings.entry((side, key.into())).or_insert_with(|| {
            bindings(
                self.documents[side]?,
                &mut self.binding_graphs[side],
                key,
                self.index.links(side),
                None,
            )
        });
        if cached.is_none() {
            let previous_providers = previous_providers();
            let doc = self.documents[side]?;
            let available = canonical_providers(&previous_providers);
            let names = doc["packages"][pin::package_key(key)]["peerDependencies"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(n, _)| n.as_str())
                .chain(
                    doc["snapshots"][key]["transitivePeerDependencies"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                );
            let projection: Vec<_> = names.map(|name| available.get(name).cloned()).collect();
            let no_evidence = projection.iter().all(Option::is_none);
            if !self
                .misses
                .entry((side, key.into()))
                .or_default()
                .insert(projection)
            {
                return None;
            }
            *cached = contextual_bindings(doc, key, &available).or_else(|| {
                if no_evidence {
                    return None;
                }
                bindings(
                    self.documents[side]?,
                    &mut self.binding_graphs[side],
                    key,
                    self.index.links(side),
                    Some(&available),
                )
            });
        }
        cached.as_ref()
    }

    pub(crate) fn provider_dependencies(&self) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
        let mut dependencies: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut aliases = BTreeSet::new();
        for doc in self.documents.into_iter().flatten() {
            for (key, package) in graph::section(doc, "packages").into_iter().flatten() {
                let Some((name, _)) = key.rsplit_once('@') else {
                    continue;
                };
                dependencies.entry(name).or_default().extend(
                    graph::section(package, "peerDependencies")
                        .into_iter()
                        .flatten()
                        .map(|(name, _)| name.as_str()),
                );
            }
            for (key, snapshot) in graph::section(doc, "snapshots").into_iter().flatten() {
                let Some((package, _)) = pin::package_key(key).rsplit_once('@') else {
                    continue;
                };
                let names = dependencies.entry(package).or_default();
                if let Some(context) = context_tokens(key) {
                    names.extend(
                        context
                            .into_iter()
                            .filter(|(name, _)| *name != "patch_hash")
                            .map(|(name, _)| name),
                    );
                }
                names.extend(
                    snapshot
                        .get("transitivePeerDependencies")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                );
                let own = doc
                    .get("packages")
                    .and_then(|packages| packages.get(pin::package_key(key)))
                    .and_then(|package| package.get("peerDependencies"));
                for group in ["dependencies", "optionalDependencies"] {
                    for (name, value) in graph::section(snapshot, group).into_iter().flatten() {
                        if own.is_some_and(|peers| peers.get(name).is_some())
                            && let Some(value) = value.as_str()
                        {
                            let actual = canonical_binding(name, value).0;
                            if actual != name {
                                dependencies.entry(name).or_default().insert(actual);
                                aliases.insert((name.as_str(), actual));
                            }
                        }
                    }
                }
            }
        }
        loop {
            let mut changed = false;
            for (alias, actual) in &aliases {
                let inherited = dependencies.get(actual).cloned().unwrap_or_default();
                let names = dependencies.entry(alias).or_default();
                let before = names.len();
                names.extend(inherited);
                changed |= before != names.len();
            }
            if !changed {
                break;
            }
        }
        dependencies
    }

    fn context_metadata(&self, side: usize, key: &str) -> Option<&Bindings> {
        let index = (side, key.to_owned());
        self.bindings
            .get(&index)
            .and_then(Option::as_ref)
            .or_else(|| self.constructed.get(&index).map(|context| &context.fixed))
    }

    /// Construct a new context from unambiguous dependency-edge evidence when
    /// an existing opaque identity cannot be decoded. This never certifies or
    /// reuses that opaque identity: rebinding must encode a new instance.
    pub(crate) fn resolution_context(
        &mut self,
        side: usize,
        key: &str,
        previous: &BTreeMap<String, String>,
    ) -> Option<BTreeMap<String, String>> {
        self.resolution_bindings(side, key, previous)
            .map(|context| std::rc::Rc::unwrap_or_clone(context.peers))
    }

    pub(crate) fn source_context(
        &mut self,
        side: usize,
        key: &str,
        previous: &BTreeMap<String, String>,
    ) -> Option<BTreeMap<String, String>> {
        self.original_bindings(side, key, previous)
            .map(|context| std::rc::Rc::unwrap_or_clone(context.peers))
    }

    pub(crate) fn original_context(
        &mut self,
        side: usize,
        key: &str,
        previous: &BTreeMap<String, String>,
    ) -> Option<std::rc::Rc<BTreeMap<String, String>>> {
        self.original_bindings(side, key, previous)
            .map(|context| context.peers)
    }

    pub(super) fn resolution_bindings(
        &mut self,
        side: usize,
        key: &str,
        previous: &BTreeMap<String, String>,
    ) -> Option<Bindings> {
        if let Some(Origin {
            side,
            original,
            bindings: peers,
        }) = self.origins.get(key)
        {
            let metadata = self.context_metadata(*side, original)?;
            return Some(Bindings {
                peers: peers.clone(),
                hash: metadata.hash,
                flat: metadata.flat,
                linked_peer_encoding: metadata.linked_peer_encoding,
                versions_only: metadata.versions_only.clone(),
            });
        }
        self.original_bindings(side, key, previous)
    }

    pub(super) fn original_bindings(
        &mut self,
        side: usize,
        key: &str,
        previous: &BTreeMap<String, String>,
    ) -> Option<Bindings> {
        let index = (side, key.to_owned());
        if let Some(context) = self.constructed.get(&index)
            && self.bindings.get(&index).is_none_or(Option::is_none)
            && let Some(context) = context.resolve(previous)
        {
            return Some(context);
        }
        if let Some(context) = self.source_bindings_with(side, key, || Cow::Borrowed(previous)) {
            return Some(context.clone());
        }
        if !self.constructed.contains_key(&index) {
            let mut alternatives = BindingCandidates::new();
            let fixed = analyze_bindings(
                self.documents[side]?,
                &mut self.binding_graphs[side],
                key,
                self.index.links(side),
                None,
                Some((
                    self.dedupe_peers,
                    self.linked_peer_encoding?,
                    &mut alternatives,
                )),
            )?;
            self.constructed.insert(
                index.clone(),
                ConstructedBindings {
                    fixed,
                    alternatives,
                },
            );
        }
        self.constructed[&index].resolve(previous)
    }
}

fn contextual_bindings(
    doc: &Value,
    key: &str,
    available: &BTreeMap<String, String>,
) -> Option<Bindings> {
    let snapshot = doc.get("snapshots")?.get(key)?;
    let package = doc.get("packages")?.get(pin::package_key(key))?;
    let mut names: BTreeSet<&str> = package
        .get("peerDependencies")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(n, _)| n.as_str())
        .collect();
    names.extend(
        snapshot
            .get("transitivePeerDependencies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    );
    let mut peers = BTreeMap::new();
    for name in names {
        let value = ["dependencies", "optionalDependencies"]
            .into_iter()
            .find_map(|g| snapshot.get(g)?.get(name)?.as_str())
            .or_else(|| available.get(name).map(String::as_str));
        let Some(value) = value else {
            continue;
        };
        peers.insert(name.into(), value.into());
    }
    if let Some(patch) = hash_in_key(key) {
        peers.insert("patch_hash".into(), patch.into());
    }
    let suffix = &key[pin::package_key(key).len()..];
    let suffix = if hash_in_key(key).is_some() {
        suffix.split_once(')')?.1
    } else {
        suffix
    };
    let expected = if suffix.is_empty() {
        ""
    } else {
        let token = suffix.strip_prefix('(')?.strip_suffix(')')?;
        if token.is_empty() {
            return None;
        }
        token
    };
    let hash = HashStyle::recognize(expected);
    for flat in [false, true] {
        if let Some(linked_peer_encoding) =
            bindings::matching_encoding(&peers, flat, hash, expected)
        {
            return Some(Bindings {
                peers: peers.into(),
                hash,
                flat,
                linked_peer_encoding,
                versions_only: BTreeSet::new(),
            });
        }
    }
    None
}
