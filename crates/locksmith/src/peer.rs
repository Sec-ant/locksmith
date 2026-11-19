//! Reconcile existing dependency instances, preserving their package identity.
use crate::{LinkedPeerEncoding, Value, graph};

mod identity;
use identity::{HashStyle, context, context_tokens, encode, link_peer_id};
pub(crate) use identity::{
    context as explicit_bindings, matches_bindings, patch, patch_hash, projected_identity,
    root_provider,
};
mod bindings;
pub(crate) use bindings::optional_requirement;
use bindings::{BindingGraph, Bindings, ConstructedBindings, analyze_bindings, bindings};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::{borrow::Cow, cell::OnceCell, rc::Rc};

type Origin = (usize, String, std::rc::Rc<BTreeMap<String, String>>);
type SavedRecord = (String, Option<Value>, Option<Origin>);

type Triple = (String, String, String);
type WorkspaceLinks = HashMap<String, HashMap<String, Option<String>>>;
type BindingCandidates = BTreeMap<String, BTreeSet<String>>;

type ReleaseCandidates<'a> = HashMap<&'a str, Vec<(&'a str, &'a Value)>>;

pub(crate) struct PeerIndex<'a> {
    documents: [Option<&'a Value>; 4],
    candidates: OnceCell<ReleaseCandidates<'a>>,
    links: [OnceCell<WorkspaceLinks>; 4],
}

impl<'a> PeerIndex<'a> {
    pub(crate) fn new(documents: [Option<&'a Value>; 4]) -> Rc<Self> {
        Rc::new(Self {
            documents,
            candidates: OnceCell::new(),
            links: Default::default(),
        })
    }

    fn candidates(&self) -> &ReleaseCandidates<'a> {
        self.candidates.get_or_init(|| {
            let mut candidates = ReleaseCandidates::new();
            for document in self.documents.into_iter().flatten() {
                for (key, body) in graph::section(document, "snapshots").into_iter().flatten() {
                    candidates
                        .entry(graph::package_key(key))
                        .or_default()
                        .push((key, body));
                }
            }
            candidates
        })
    }

    fn links(&self, side: usize) -> &WorkspaceLinks {
        self.links[side].get_or_init(|| {
            let mut links = WorkspaceLinks::new();
            for (_, body) in self.documents[side]
                .and_then(|doc| graph::section(doc, "snapshots"))
                .into_iter()
                .flatten()
            {
                for group in ["dependencies", "optionalDependencies"] {
                    for (name, value) in graph::section(body, group).into_iter().flatten() {
                        let Some(reference) = value.as_str() else {
                            continue;
                        };
                        let Some(path) = reference.strip_prefix("link:") else {
                            continue;
                        };
                        for encoding in [LinkedPeerEncoding::Filename, LinkedPeerEncoding::Path] {
                            links
                                .entry(name.clone())
                                .or_default()
                                .entry(link_peer_id(path, encoding))
                                .and_modify(|old| {
                                    if old.as_deref() != Some(reference) {
                                        *old = None;
                                    }
                                })
                                .or_insert_with(|| Some(reference.into()));
                        }
                    }
                }
            }
            links
        })
    }
}

pub(crate) struct Merger<'a> {
    documents: [Option<&'a Value>; 4],
    cache: HashMap<Triple, Option<String>>,
    active: HashSet<Triple>,
    rebinding: HashSet<String>,
    pub generated: BTreeMap<String, Value>,
    bindings: HashMap<(usize, String), Option<Bindings>>,
    misses: HashMap<(usize, String), HashSet<Vec<Option<String>>>>,
    constructed: HashMap<(usize, String), ConstructedBindings>,
    binding_graphs: [BindingGraph<'a>; 4],
    index: Rc<PeerIndex<'a>>,
    preferred_sources: Option<[usize; 4]>,
    dedupe_peers: bool,
    linked_peer_encoding: Option<LinkedPeerEncoding>,
    origins: HashMap<String, Origin>,
    provisional: Option<Vec<SavedRecord>>,
}

pub(crate) fn choose<'a, T: PartialEq + ?Sized>(
    base: Option<&'a T>,
    ours: Option<&'a T>,
    theirs: Option<&'a T>,
) -> Option<Option<&'a T>> {
    if ours == theirs {
        Some(ours)
    } else if ours == base {
        Some(theirs)
    } else if theirs == base {
        Some(ours)
    } else {
        None
    }
}

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
            .map(|(side, key, _)| (*side, key.clone()));
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
                && !(source.versions_only.contains(name) && graph::package_key(provider) == binding)
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
        let records = self.provisional.take().expect("active importer pass");
        if settled {
            return;
        }
        for (key, body, origin) in records.into_iter().rev() {
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

    pub(crate) fn has_release(&self, key: &str) -> bool {
        self.index.candidates().contains_key(key)
    }

    pub(crate) fn origin_side(&self, key: &str) -> Option<usize> {
        self.origins.get(key).map(|(side, _, _)| *side)
    }

    pub(crate) fn origin_key<'k>(&'k self, key: &'k str) -> &'k str {
        self.origins
            .get(key)
            .map_or(key, |(_, original, _)| original)
    }

    pub(crate) fn discard_generated(&mut self, key: &str) {
        self.generated.remove(key);
        self.origins.remove(key);
    }

    pub(crate) fn repatch(&mut self, side: usize, key: &str, hash: Option<&str>) -> Option<String> {
        if patch(key) == hash {
            return Some(key.into());
        }
        let mut bindings = self.binding_context(side, key, &BTreeMap::new())?;
        if let Some(hash) = hash {
            bindings.insert("patch_hash".into(), hash.into());
        } else {
            bindings.remove("patch_hash");
        }
        let package = graph::package_key(key);
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
            .map_or((side, key.to_owned()), |(side, original, _)| {
                (*side, original.clone())
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
        self.origins
            .insert(key.into(), (side, original.into(), bindings.into()));
    }

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
            &doc["packages"][graph::package_key(candidate)],
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
        if let Some((_, _, bindings)) = self.origins.get(key) {
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
            let names = doc["packages"][graph::package_key(key)]["peerDependencies"]
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
                let Some((package, _)) = graph::package_key(key).rsplit_once('@') else {
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
                    .and_then(|packages| packages.get(graph::package_key(key)))
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

    fn resolution_bindings(
        &mut self,
        side: usize,
        key: &str,
        previous: &BTreeMap<String, String>,
    ) -> Option<Bindings> {
        if let Some((side, original, peers)) = self.origins.get(key) {
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

    fn original_bindings(
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

    pub fn new([base, ours, theirs]: [Option<&'a Value>; 3]) -> Self {
        Self::with_index(PeerIndex::new([base, ours, theirs, None]))
    }

    pub(crate) fn with_index(index: Rc<PeerIndex<'a>>) -> Self {
        let documents = index.documents;
        Self {
            preferred_sources: None,
            dedupe_peers: false,
            linked_peer_encoding: None,
            documents,
            index,
            bindings: HashMap::new(),
            misses: HashMap::new(),
            constructed: HashMap::new(),
            binding_graphs: Default::default(),
            cache: HashMap::new(),
            active: HashSet::new(),
            rebinding: HashSet::new(),
            generated: BTreeMap::new(),
            origins: HashMap::new(),
            provisional: None,
        }
    }

    pub fn dependency(
        &mut self,
        name: &str,
        b: Option<&Value>,
        o: Option<&Value>,
        t: Option<&Value>,
    ) -> Option<Value> {
        let (b, o, t) = (b?, o?, t?);
        let specifier = choose(b.get("specifier"), o.get("specifier"), t.get("specifier"))??;
        if o.as_object()?.len() != 2 || t.as_object()?.len() != 2 {
            return None;
        }
        let version = self.reference(
            name,
            b.get("version")?.as_str()?,
            o.get("version")?.as_str()?,
            t.get("version")?.as_str()?,
        )?;
        if o.get("specifier") != t.get("specifier") {
            let range = specifier.as_str()?.parse::<node_semver::Range>().ok()?;
            let resolved = version
                .split('(')
                .next()?
                .parse::<node_semver::Version>()
                .ok()?;
            if !resolved.satisfies(&range) {
                return None;
            }
        }
        let mut result = o.clone();
        result["specifier"] = specifier.clone();
        result["version"] = Value::String(version);
        Some(result)
    }

    pub fn snapshot(
        &mut self,
        key: &str,
        b: Option<&Value>,
        o: Option<&Value>,
        t: Option<&Value>,
    ) -> Option<Option<Value>> {
        let merged = self.value(None, b, o, t)?;
        if let Some(value) = &merged {
            for (side, doc) in self.documents.into_iter().enumerate() {
                let Some(doc) = doc else { continue };
                if doc.get("snapshots").and_then(|m| m.get(key)).is_some()
                    && let Some(context) = bindings(
                        doc,
                        &mut self.binding_graphs[side],
                        key,
                        self.index.links(side),
                        None,
                    )
                {
                    let mut changed = std::rc::Rc::unwrap_or_clone(context.peers);
                    for group in ["dependencies", "optionalDependencies"] {
                        for (name, actual) in graph::section(value, group).into_iter().flatten() {
                            if [o, t].into_iter().flatten().any(|record| {
                                record.get(group).and_then(|m| m.get(name)) == Some(actual)
                            }) {
                                changed.remove(name);
                            }
                        }
                    }
                    Self::check_bindings(
                        self.documents.into_iter().flatten(),
                        graph::package_key(key),
                        &changed,
                        value,
                    )?;
                    break;
                }
            }
        }
        Some(merged)
    }

    pub fn reference(
        &mut self,
        name: &str,
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Option<String> {
        if let Some(value) = choose(Some(base), Some(ours), Some(theirs)) {
            return value.map(str::to_owned);
        }
        let keys: Vec<_> = [base, ours, theirs]
            .into_iter()
            .zip(self.documents)
            .map(|(r, d)| {
                graph::snapshot_key(name, r, d.and_then(|d| graph::section(d, "snapshots")))
            })
            .collect();
        let triple = (keys[0].clone(), keys[1].clone(), keys[2].clone());
        if let Some(result) = self.cache.get(&triple) {
            return result.as_ref().map(|k| reference(name, k));
        }
        // A cycle with conflicting identities has no finite local proof. Retain
        // its conflict instead of choosing an arbitrary traversal order.
        if !self.active.insert(triple.clone()) {
            return None;
        }
        let result = self.instance(&keys);
        self.active.remove(&triple);
        self.cache.insert(triple, result.clone());
        result.map(|key| reference(name, &key))
    }

    fn instance(&mut self, keys: &[String]) -> Option<String> {
        let packages: Vec<_> = keys.iter().map(|k| graph::package_key(k)).collect();
        // A new package release can have different dependency declarations.
        // Rebind that release's graph without copying old-release dependencies
        // or substituting a different consumer's regular resolutions.
        let package = choose(Some(packages[0]), Some(packages[1]), Some(packages[2]))??;
        let contexts: Option<Vec<_>> = keys
            .iter()
            .enumerate()
            .map(|(side, key)| {
                let cache_key = (side, key.clone());
                self.bindings
                    .entry(cache_key)
                    .or_insert_with(|| {
                        bindings(
                            self.documents[side]?,
                            &mut self.binding_graphs[side],
                            key,
                            self.index.links(side),
                            None,
                        )
                    })
                    .clone()
            })
            .collect();
        let desired = if let Some(contexts) = contexts {
            let mut merged = BTreeMap::new();
            let names: BTreeSet<_> = contexts.iter().flat_map(|c| c.peers.keys()).collect();
            for name in names {
                let values: Vec<_> = contexts
                    .iter()
                    .map(|c| c.peers.get(name).map(String::as_str))
                    .collect();
                let value = if let Some(v) = choose(values[0], values[1], values[2]) {
                    v.map(str::to_owned)
                } else if name != "patch_hash" {
                    Some(self.reference(name, values[0]?, values[1]?, values[2]?)?)
                } else {
                    return None;
                };
                if let Some(value) = value {
                    merged.insert(name.clone(), value);
                }
            }
            let style = contexts.iter().filter_map(|c| c.hash).max();
            let flat = contexts.iter().any(|c| c.flat);
            let encoding = contexts
                .iter()
                .map(|c| c.linked_peer_encoding)
                .max()
                .expect("input contexts");
            let key = encode(
                package,
                &merged,
                style,
                flat,
                self.linked_peer_encoding.unwrap_or(encoding),
            );
            Some((key, merged))
        } else {
            None
        };
        let records: Option<Vec<_>> = self
            .documents
            .iter()
            .zip(keys)
            .map(|(d, k)| (*d)?.get("snapshots")?.get(k))
            .collect();
        let records = records?;
        if packages.iter().any(|p| *p != package)
            || keys.iter().any(|k| patch(k) != patch(&keys[0]))
        {
            let (_, peers) = desired?;
            let mut rebound = None;
            for (side, source) in packages.iter().enumerate() {
                if *source == package
                    && patch(&keys[side]) == peers.get("patch_hash").map(String::as_str)
                {
                    let key = self.rebind(side, &keys[side], &peers, &BTreeMap::new())?;
                    if rebound.as_ref().is_some_and(|previous| *previous != key) {
                        return None;
                    }
                    rebound = Some(key);
                }
            }
            return rebound;
        }
        let merged = self.value(None, Some(records[0]), Some(records[1]), Some(records[2]))??;
        if let Some((key, bindings)) = desired {
            Self::check_bindings(
                self.documents.into_iter().flatten(),
                package,
                &bindings,
                &merged,
            )?;
            if let Some(existing) = self.generated.get(&key) {
                if existing != &merged {
                    return None;
                }
            } else {
                self.generated.insert(key.clone(), merged);
            }
            Some(key)
        } else {
            // Opaque peer hashes are identities, not strings to splice. Reuse
            // an existing identity only when its entire instance matches.
            let matches: BTreeSet<_> = self
                .index
                .candidates()
                .get(package)?
                .iter()
                .filter(|(_, v)| **v == merged)
                .map(|(k, _)| *k)
                .collect();
            if matches.len() != 1 {
                return None;
            }
            Some((*matches.first()?).to_owned())
        }
    }

    // A release upgrade keeps that release's regular dependency resolutions.
    // Only inherited peer bindings are propagated through its existing graph.
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
        if verified && source.peers.as_ref() == target {
            let body = self.documents[side]?.get("snapshots")?.get(key)?;
            self.save_rebound(side, key, target, key, body.clone())?;
            return Some(key.into());
        }
        if source
            .peers
            .keys()
            .filter(|name| name.as_str() != "patch_hash")
            .ne(target.keys().filter(|name| name.as_str() != "patch_hash"))
        {
            return None;
        }
        let desired = source.identity(
            key,
            target,
            self.dedupe_peers,
            self.linked_peer_encoding
                .unwrap_or(source.linked_peer_encoding),
        );
        if !self.rebinding.insert(desired.clone()) {
            // The identity is determined before traversing the body. A back
            // edge can reference it while the owning frame finishes the node.
            return Some(desired);
        }
        let result = self.rebind_body(side, key, &source, target, &desired, previous_providers);
        self.rebinding.remove(&desired);
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
        let package = graph::package_key(key);
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
                        if let Some(mut binding) = bound_reference(name, value.as_str()?, &target) {
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
                                .is_some_and(|v| v == binding || v == graph::package_key(binding))
                            && let Some(new) = target.get(peer)
                        {
                            *binding = new.clone();
                        }
                    }
                    if next != old {
                        let child = self.recontextualize_source(side, &child, &next, scope)?;
                        *value = Value::String(crate::peer::reference(name, &child));
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
        if graph::package_key(original) == binding {
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
        let origin = self
            .origins
            .insert(key.into(), (side, original.into(), bindings.clone().into()));
        if let Some(records) = &mut self.provisional {
            records.push((key.into(), previous, origin));
        }
        Some(())
    }

    fn check_bindings<'d>(
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
                let Some(binding) = bindings
                    .get(name)
                    .cloned()
                    .or_else(|| actual.and_then(|actual| bound_reference(name, actual, bindings)))
                else {
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

    // Snapshot maps merge by dependency edge; flags remain ordinary three-way
    // values. Each recursive reference is memoized across workspace consumers.
    pub fn value(
        &mut self,
        field: Option<&str>,
        b: Option<&Value>,
        o: Option<&Value>,
        t: Option<&Value>,
    ) -> Option<Option<Value>> {
        if let Some(v) = choose(b, o, t) {
            return Some(v.cloned());
        }
        if field == Some("transitivePeerDependencies") {
            let arrays: Option<Vec<_>> = [b, o, t]
                .into_iter()
                .map(|v| match v {
                    None => Some(BTreeSet::new()),
                    Some(v) => v
                        .as_array()?
                        .iter()
                        .map(Value::as_str)
                        .collect::<Option<BTreeSet<_>>>(),
                })
                .collect();
            let arrays = arrays?;
            let names: BTreeSet<_> = arrays.iter().flat_map(|s| s.iter().copied()).collect();
            let selected: Vec<_> = names
                .into_iter()
                .filter(|n| {
                    let [b, o, t] = [
                        arrays[0].contains(n),
                        arrays[1].contains(n),
                        arrays[2].contains(n),
                    ];
                    if o == t || t == b { o } else { t }
                })
                .map(|s| Value::String(s.into()))
                .collect();
            return Some(Some(Value::Array(selected)));
        }
        if let (Some(o), Some(t)) = (o.and_then(Value::as_object), t.and_then(Value::as_object)) {
            let b = b.and_then(Value::as_object);
            let names: BTreeSet<_> = b
                .into_iter()
                .flat_map(|m| m.keys())
                .chain(o.keys())
                .chain(t.keys())
                .collect();
            let mut result = serde_json::Map::new();
            for name in names {
                let values = [b.and_then(|m| m.get(name)), o.get(name), t.get(name)];
                let v = if matches!(field, Some("dependencies" | "optionalDependencies")) {
                    if let Some(v) = choose(values[0], values[1], values[2]) {
                        v.cloned()
                    } else {
                        Some(Value::String(self.reference(
                            name,
                            values[0]?.as_str()?,
                            values[1]?.as_str()?,
                            values[2]?.as_str()?,
                        )?))
                    }
                } else {
                    self.value(Some(name), values[0], values[1], values[2])?
                };
                if let Some(v) = v {
                    result.insert(name.clone(), v);
                }
            }
            return Some(Some(Value::Object(result)));
        }
        None
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
    let (parent_name, parent_version) = graph::package_key(key).rsplit_once('@')?;
    let mut selected = None;
    for group in ["dependencies", "optionalDependencies"] {
        for (name, old) in graph::section(existing, group).into_iter().flatten() {
            let new = candidate.get(group)?.get(name)?;
            if new == old {
                continue;
            }
            let (child_name, old) = canonical_binding(name, old.as_str()?);
            let (_, new) = canonical_binding(name, new.as_str()?);
            if parent.get(child_name).copied() != Some(graph::package_key(old)) {
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
                        graph::package_key(reference) == graph::package_key(other)
                            && patch(reference) == patch(other)
                    })
            })
}

// pnpm checks peers with Yarn's satisfiesWithPrereleases: first allow
// prereleases within comparator bounds, then compare release cores of both
// the version and comparator endpoints. Ordinary dependency ranges stay strict.
pub(crate) fn satisfies_peer(version: &node_semver::Version, wanted: &str) -> bool {
    let wanted = wanted.strip_prefix("workspace:").unwrap_or(wanted);
    let Ok(range) = wanted.parse::<node_semver::Range>() else {
        return false;
    };
    if version.satisfies(&range) {
        return true;
    }
    if version.is_prerelease()
        && version
            .to_string()
            .parse::<node_semver::Range>()
            .is_ok_and(|exact| range.allows_any(&exact))
    {
        return true;
    }
    let normalized = range.to_string();
    if !version.is_prerelease() && !normalized.contains('-') {
        return false;
    }
    let release_range = normalized
        .split_whitespace()
        .map(|part| part.split_once('-').map_or(part, |(release, _)| release))
        .collect::<Vec<_>>()
        .join(" ");
    let release = node_semver::Version::new(version.major, version.minor, version.patch);
    release_range
        .parse::<node_semver::Range>()
        .is_ok_and(|range| release.satisfies(&range))
}

pub(crate) fn importer_providers(
    importers: &Value,
    id: &str,
    inherit_root: bool,
) -> BTreeMap<String, String> {
    let mut scope = BTreeMap::new();
    for path in [(inherit_root && id != ".").then_some("."), Some(id)]
        .into_iter()
        .flatten()
    {
        for group in crate::manifest::GROUPS {
            for (name, dep) in graph::section(&importers[path], group)
                .into_iter()
                .flatten()
            {
                if let Some(pin) = dep["version"].as_str() {
                    scope.insert(name.clone(), root_provider(path, pin));
                }
            }
        }
    }
    scope
}

pub(crate) fn canonical_providers(
    providers: &BTreeMap<String, String>,
) -> Cow<'_, BTreeMap<String, String>> {
    let mut result = Cow::Borrowed(providers);
    for value in providers.values() {
        if value.starts_with("link:") {
            continue;
        }
        if let Some((name, _)) = graph::package_key(value).rsplit_once('@') {
            if providers.contains_key(name) {
                continue;
            }
            let version = &value[name.len() + 1..];
            let replace = result.get(name).is_none_or(|old| {
                matches!((graph::package_key(old).parse::<node_semver::Version>(), graph::package_key(version).parse::<node_semver::Version>()),
                    (Ok(old), Ok(new)) if new > old)
            });
            if replace {
                result.to_mut().insert(name.into(), version.into());
            }
        }
    }
    result
}

pub(crate) fn bound_reference(
    name: &str,
    actual: &str,
    bindings: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(value) = bindings.get(name) {
        return Some(value.clone());
    }
    let (target, _) = graph::package_key(actual).rsplit_once('@')?;
    bindings
        .get(target)
        .map(|value| format!("{target}@{value}"))
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
                        .and_then(|r| graph::package_key(r).rsplit_once('@'))
                        .is_some_and(|(actual, _)| changed.contains(actual))
                })
    };
    doc.get("packages")
        .and_then(|v| v.get(graph::package_key(key)))
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

fn contextual_bindings(
    doc: &Value,
    key: &str,
    available: &BTreeMap<String, String>,
) -> Option<Bindings> {
    let snapshot = doc.get("snapshots")?.get(key)?;
    let package = doc.get("packages")?.get(graph::package_key(key))?;
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
    if let Some(patch) = patch(key) {
        peers.insert("patch_hash".into(), patch.into());
    }
    let suffix = &key[graph::package_key(key).len()..];
    let suffix = if patch(key).is_some() {
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

fn reference(name: &str, key: &str) -> String {
    key.strip_prefix(name)
        .and_then(|v| v.strip_prefix('@'))
        .unwrap_or(key)
        .to_owned()
}

pub(crate) fn canonical_binding<'a>(name: &'a str, reference: &'a str) -> (&'a str, &'a str) {
    if reference.starts_with("link:") {
        return (name, reference);
    }
    if let Some((actual, _)) = graph::package_key(reference).rsplit_once('@') {
        (actual, &reference[actual.len() + 1..])
    } else {
        (name, reference)
    }
}
