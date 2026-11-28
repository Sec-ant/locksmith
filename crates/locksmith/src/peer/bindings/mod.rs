//! Recover a snapshot’s original peer bindings and construct replacements from scoped evidence.
use super::identity::{
    context_tokens, encode, link_peer_id, peer_text, token_matches, token_reference,
};
use super::{BindingCandidates, WorkspaceLinks, canonical_binding, canonical_providers};
use crate::{
    LinkedPeerEncoding, Value,
    model::checksum::{HashStyle, digest},
    model::graph,
    model::patch::hash_in_key,
    model::pin,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

mod search;

#[derive(Clone)]
pub(super) struct Bindings {
    pub(super) peers: std::rc::Rc<BTreeMap<String, String>>,
    pub(super) hash: Option<HashStyle>,
    pub(super) flat: bool,
    pub(super) linked_peer_encoding: LinkedPeerEncoding,
    // A deduplicated hash can prove a version shared by several nested peer
    // contexts without selecting one of those contexts as the provider.
    pub(super) versions_only: BTreeSet<String>,
}

pub(super) struct ConstructedBindings {
    pub(super) fixed: Bindings,
    pub(super) alternatives: BindingCandidates,
}

impl Bindings {
    pub(super) fn identity(
        &self,
        key: &str,
        target: &BTreeMap<String, String>,
        dedupe: bool,
        encoding: LinkedPeerEncoding,
    ) -> String {
        let package = pin::package_key(key);
        if dedupe || self.hash.is_some() {
            return encode(package, target, self.hash, dedupe || self.flat, encoding);
        }
        let Some(tokens) = context_tokens(key) else {
            return encode(package, target, self.hash, self.flat, encoding);
        };
        let partially_flat = self.flat && tokens.iter().any(|(_, value)| value.contains('('));
        if self.flat && !partially_flat {
            return encode(package, target, None, true, encoding);
        }
        let mut shortened = std::borrow::Cow::Borrowed(target);
        for (name, value) in target {
            let Some(before) = self.peers.get(name) else {
                continue;
            };
            if !value.contains('(') || value == before && !partially_flat {
                continue;
            }
            let (actual, reference) = canonical_binding(name, before);
            if !tokens.contains(&(actual, pin::package_key(reference))) {
                continue;
            }
            // The token for a cyclic provider stops at its release even when
            // its concrete edge has acquired a nested peer context.
            let closes_cycle = || {
                let Some((package_name, version)) = package.rsplit_once('@') else {
                    return false;
                };
                context_tokens(value).is_some_and(|peers| {
                    peers.iter().any(|(peer, bound)| {
                        *peer == package_name && pin::package_key(bound) == version
                    })
                })
            };
            if partially_flat && reference.contains('(') || closes_cycle() {
                shortened
                    .to_mut()
                    .insert(name.clone(), pin::package_key(value).into());
            }
        }
        encode(package, &shortened, None, false, encoding)
    }
}

impl ConstructedBindings {
    pub(super) fn resolve(&self, providers: &BTreeMap<String, String>) -> Option<Bindings> {
        let mut context = self.fixed.clone();
        if self.alternatives.is_empty() {
            return Some(context);
        }
        let providers = canonical_providers(providers);
        for (name, candidates) in &self.alternatives {
            let provider = providers.get(name)?;
            let selected = if let Some(exact) = candidates.get(provider) {
                exact
            } else {
                let mut matching = candidates.iter().filter(|candidate| {
                    self.fixed.flat
                        && !provider.starts_with("link:")
                        && !candidate.starts_with("link:")
                        && pin::package_key(provider) == pin::package_key(candidate)
                        && hash_in_key(provider) == hash_in_key(candidate)
                });
                match (matching.next(), matching.next()) {
                    (Some(first), None) => first,
                    (None, _) if self.fixed.flat => {
                        // A flattened opaque context may inherit a provider
                        // absent from its deduplicated descendants. The enclosing
                        // binding constructs a new identity; it does not verify
                        // the old hash or change those descendants' local pins.
                        provider
                    }
                    _ => return None,
                }
            };
            std::rc::Rc::make_mut(&mut context.peers).insert(name.clone(), selected.clone());
        }
        Some(context)
    }
}
pub(crate) fn optional_requirement(
    key: &str,
    name: &str,
    snapshots: &serde_json::Map<String, Value>,
    packages: &serde_json::Map<String, Value>,
) -> Option<bool> {
    let mut pending = vec![key.to_owned()];
    let mut seen = BTreeSet::new();
    let mut found = false;
    while let Some(key) = pending.pop() {
        if !seen.insert(key.clone()) {
            continue;
        }
        let Some(package) = packages.get(pin::package_key(&key)) else {
            return Some(false);
        };
        let own = package.get("peerDependencies");
        if own.is_some_and(|peers| peers.get(name).is_some()) {
            found = true;
            if package["peerDependenciesMeta"][name]["optional"] != true {
                return Some(false);
            }
            continue;
        }
        let Some(body) = snapshots.get(&key) else {
            return Some(false);
        };
        if ["dependencies", "optionalDependencies"]
            .into_iter()
            .any(|group| {
                graph::section(body, group)
                    .into_iter()
                    .flatten()
                    .any(|(edge, value)| {
                        value
                            .as_str()
                            .is_some_and(|value| canonical_binding(edge, value).0 == name)
                    })
            })
        {
            continue;
        }
        for (dependency, value) in ["dependencies", "optionalDependencies"]
            .into_iter()
            .flat_map(|group| {
                graph::section(body, group)
                    .into_iter()
                    .flatten()
                    .map(|(n, v)| (n.as_str(), v))
            })
        {
            let Some(value) = value.as_str() else {
                return Some(false);
            };
            if !value.starts_with("link:")
                && own.is_none_or(|peers| peers.get(dependency).is_none())
            {
                let child = graph::snapshot_key(dependency, value, Some(snapshots));
                let Some(record) = snapshots.get(&child) else {
                    return Some(false);
                };
                let Some(package) = packages.get(pin::package_key(&child)) else {
                    return Some(false);
                };
                if package
                    .get("peerDependencies")
                    .is_some_and(|peers| peers.get(name).is_some())
                    || record
                        .get("transitivePeerDependencies")
                        .and_then(Value::as_array)
                        .is_some_and(|names| names.iter().any(|n| n == name))
                {
                    pending.push(child);
                }
            }
        }
    }
    found.then_some(true)
}

fn named_context(
    doc: &Value,
    key: &str,
    tokens: &[(&str, &str)],
    links: &WorkspaceLinks,
) -> Option<Bindings> {
    if tokens.iter().all(|(name, _)| *name == "patch_hash") {
        return Some(Bindings {
            peers: tokens
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect::<BTreeMap<_, _>>()
                .into(),
            hash: None,
            flat: false,
            linked_peer_encoding: LinkedPeerEncoding::Filename,
            versions_only: BTreeSet::new(),
        });
    }
    let snapshot = doc.get("snapshots")?.get(key)?;
    let package = doc.get("packages")?.get(pin::package_key(key))?;
    let names: BTreeSet<_> = package
        .get("peerDependencies")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, _)| name.as_str())
        .chain(
            snapshot
                .get("transitivePeerDependencies")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .collect();
    let mut used = vec![false; tokens.len()];
    let mut peers = BTreeMap::new();
    let mut flat = false;
    for name in &names {
        let actual = ["dependencies", "optionalDependencies"]
            .into_iter()
            .find_map(|group| snapshot.get(group)?.get(*name)?.as_str());
        let Some(actual) = actual else {
            continue;
        };
        if let Some((index, (_, encoded))) = tokens
            .iter()
            .enumerate()
            .find(|(index, (n, v))| !used[*index] && token_matches(name, actual, n, v))
        {
            used[index] = true;
            flat |= !actual.starts_with("link:") && *encoded != canonical_binding(name, actual).1;
            peers.insert((*name).into(), actual.into());
        }
    }
    for ((name, value), used) in tokens.iter().zip(used) {
        if used {
            continue;
        }
        if *name != "patch_hash" && !names.is_empty() && !names.contains(name) {
            return None;
        }
        if peers
            .insert((*name).into(), token_reference(links, name, value)?)
            .is_some()
        {
            return None;
        }
    }
    let linked_peer_encoding = [LinkedPeerEncoding::Filename, LinkedPeerEncoding::Path]
        .into_iter()
        .find(|encoding| {
            peers.iter().all(|(name, value)| {
                value.strip_prefix("link:").is_none_or(|path| {
                    let encoded = link_peer_id(path, *encoding);
                    tokens.iter().any(|(n, v)| *n == name && *v == encoded)
                })
            })
        })?;
    Some(Bindings {
        peers: peers.into(),
        hash: None,
        flat,
        linked_peer_encoding,
        versions_only: BTreeSet::new(),
    })
}

pub(super) fn matching_encoding(
    peers: &BTreeMap<String, String>,
    flat: bool,
    style: Option<HashStyle>,
    expected: &str,
) -> Option<LinkedPeerEncoding> {
    let has_links = peers.values().any(|value| value.starts_with("link:"));
    [LinkedPeerEncoding::Filename, LinkedPeerEncoding::Path]
        .into_iter()
        .take(if has_links { 2 } else { 1 })
        .find(|encoding| {
            let text = peer_text(peers, flat, *encoding);
            let actual = match style {
                Some(style) => digest(&text, style),
                None => text,
            };
            actual == expected
        })
}

// Resolve source edges once. A peer search tracks compact node indices, even
// when the same long snapshot identity occurs along many paths.
#[derive(Default)]
pub(super) struct BindingGraph<'a> {
    snapshots: Option<SnapshotGraph<'a>>,
    parallelism: std::cell::OnceCell<usize>,
    inherited: HashMap<&'a str, BTreeMap<&'a str, BTreeSet<String>>>,
}

struct SnapshotGraph<'a> {
    index: HashMap<&'a str, usize>,
    nodes: Vec<SnapshotNode<'a>>,
    edges: Vec<(usize, bool)>,
}

struct SnapshotNode<'a> {
    key: &'a str,
    record: &'a Value,
    own: Option<&'a serde_json::Map<String, Value>>,
    edges: std::ops::Range<usize>,
}

impl<'a> SnapshotGraph<'a> {
    fn new(doc: &'a Value) -> Self {
        let records = graph::section(doc, "snapshots").expect("source snapshot");
        let index: HashMap<_, _> = records
            .keys()
            .enumerate()
            .map(|(id, key)| (key.as_str(), id))
            .collect();
        let mut nodes = Vec::with_capacity(records.len());
        let mut edges = Vec::new();
        for (key, record) in records {
            let own = graph::section(&doc["packages"][pin::package_key(key)], "peerDependencies");
            let start = edges.len();
            for group in ["dependencies", "optionalDependencies"] {
                for (name, value) in graph::section(record, group).into_iter().flatten() {
                    let Some(reference) = value.as_str() else {
                        continue;
                    };
                    let named = format!("{name}@{reference}");
                    if let Some(&child) = index.get(named.as_str()).or_else(|| index.get(reference))
                    {
                        edges.push((child, own.is_some_and(|peers| peers.contains_key(name))));
                    }
                }
            }
            nodes.push(SnapshotNode {
                key,
                record,
                own,
                edges: start..edges.len(),
            });
        }
        Self {
            index,
            nodes,
            edges,
        }
    }
}

pub(super) fn bindings<'a>(
    doc: &'a Value,
    graph: &mut BindingGraph<'a>,
    key: &str,
    links: &WorkspaceLinks,
    providers: Option<&BTreeMap<String, String>>,
) -> Option<Bindings> {
    analyze_bindings(doc, graph, key, links, providers, None)
}

pub(super) fn analyze_bindings<'a>(
    doc: &'a Value,
    graph: &mut BindingGraph<'a>,
    key: &str,
    links: &WorkspaceLinks,
    providers: Option<&BTreeMap<String, String>>,
    reconstruction: Option<(bool, LinkedPeerEncoding, &mut BindingCandidates)>,
) -> Option<Bindings> {
    let tokens = context_tokens(key);
    if let Some(tokens) = &tokens
        && let Some(context) = named_context(doc, key, tokens, links)
    {
        return Some(context);
    }
    let mut suffix = &key[pin::package_key(key).len()..];
    let mut peers = BTreeMap::new();
    if suffix.starts_with("(patch_hash=") {
        let end = suffix.find(')')?;
        peers.insert("patch_hash".into(), suffix[12..end].into());
        suffix = &suffix[end + 1..];
    }
    let (style, hash) = if let Some(tokens) = tokens {
        let mut ids: Vec<_> = tokens
            .into_iter()
            .filter(|(n, _)| *n != "patch_hash")
            .map(|(n, v)| format!("{n}@{v}"))
            .collect();
        ids.sort_unstable();
        (None, ids.join(")("))
    } else {
        let hash = suffix.strip_prefix('(')?.strip_suffix(')')?;
        let style = HashStyle::recognize(hash)?;
        (Some(style), hash.to_owned())
    };
    let snapshot = doc.get("snapshots")?.get(key)?;
    let mut names: BTreeSet<&str> = doc
        .get("packages")?
        .get(pin::package_key(key))?
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
    let binding = |record: &Value, name: &str| {
        record
            .get("dependencies")
            .and_then(|v| v.get(name))
            .or_else(|| record.get("optionalDependencies").and_then(|v| v.get(name)))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let mut bound = BTreeSet::new();
    for name in &names {
        if let Some(value) = binding(snapshot, name) {
            bound.insert(*name);
            peers.insert((*name).into(), value);
        }
    }
    for flat in [false, true] {
        if let Some(linked_peer_encoding) = matching_encoding(&peers, flat, style, &hash) {
            return Some(Bindings {
                peers: peers.into(),
                hash: style,
                flat,
                linked_peer_encoding,
                versions_only: BTreeSet::new(),
            });
        }
    }
    // Follow inherited peer scopes, stopping at a child's explicit context or
    // local provider. Searching every descendant also collects unrelated local
    // versions and peers which are missing in this consumer's scope.
    let missing: BTreeSet<_> = names.into_iter().filter(|n| !bound.contains(n)).collect();
    let mut found: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let cached = if reconstruction.is_none() && providers.is_some() {
        graph.inherited.get(key).cloned()
    } else {
        None
    };
    if let Some(cached) = cached {
        found = cached;
    } else if !missing.is_empty() {
        let graph = graph
            .snapshots
            .get_or_insert_with(|| SnapshotGraph::new(doc));
        let root = *graph.index.get(key)?;
        let mut seen = vec![0; graph.nodes.len()];
        let mut pending = Vec::new();
        for (index, &name) in missing.iter().enumerate() {
            let generation = index + 1;
            pending.push(root);
            while let Some(id) = pending.pop() {
                if seen[id] == generation {
                    continue;
                }
                seen[id] = generation;
                let node = &graph.nodes[id];
                let child = node.key;
                let record = node.record;
                if id != root {
                    if reconstruction.is_none()
                        && let Some(context) = context_tokens(child)
                    {
                        if let Some(value) = binding(record, name) {
                            if context
                                .iter()
                                .any(|(n, v)| token_matches(name, &value, n, v))
                            {
                                found.entry(name).or_default().insert(value);
                            }
                            continue;
                        }
                        if let Some((_, value)) = context.iter().find(|(n, _)| *n == name) {
                            found
                                .entry(name)
                                .or_default()
                                .insert(token_reference(links, name, value)?);
                            continue;
                        }
                        // An alias is encoded under its actual package name. Follow
                        // its declaration to recover that mapping, while preserving
                        // this explicit context's boundary for absent optional peers.
                        let aliases = transitive_aliases(doc, child, name, &context);
                        if !aliases.is_empty() {
                            found.entry(name).or_default().extend(aliases);
                        }
                        continue;
                    }
                    if let Some(value) = binding(record, name) {
                        if node.own.is_some_and(|own| own.contains_key(name)) {
                            found.entry(name).or_default().insert(value);
                        }
                        continue;
                    }
                    if !record["transitivePeerDependencies"]
                        .as_array()
                        .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(name)))
                    {
                        continue;
                    }
                }
                for &(child, supplied) in &graph.edges[node.edges.clone()] {
                    if reconstruction.is_none() || !supplied {
                        pending.push(child);
                    }
                }
            }
        }
    }
    if reconstruction.is_none() && providers.is_none() && !missing.is_empty() {
        let snapshots = graph.snapshots.as_ref().expect("traversed source graph");
        graph
            .inherited
            .insert(snapshots.nodes[snapshots.index[key]].key, found.clone());
    }
    if let Some(providers) = providers {
        // The context-free attempt already searched the graph's candidates.
        let mut extended = false;
        for name in missing {
            if let Some(value) = providers.get(name) {
                extended |= found.entry(name).or_default().insert(value.clone());
            }
        }
        if !extended {
            return None;
        }
    }
    if let Some(tokens) = context_tokens(key) {
        // Explicit identities already identify the participating releases.
        // Optional peers reused only by descendants cannot add candidates to
        // this instance; aliases still need their declared-name mapping.
        found.retain(|name, values| {
            values.retain(|value| tokens.iter().any(|(n, v)| token_matches(name, value, n, v)));
            !values.is_empty()
        });
    }
    for values in found.values_mut() {
        // A flattened suffix and an edge can describe the same provider. Keep
        // the full edge when it is the sole context for that package version.
        let nested: BTreeSet<_> = values
            .iter()
            .filter(|v| pin::package_key(v) != *v)
            .map(|v| pin::package_key(v).to_owned())
            .collect();
        values.retain(|value| !nested.contains(value));
    }
    let direct = peers;
    if let Some((flat, linked_peer_encoding, alternatives)) = reconstruction {
        let mut peers = direct;
        let own = doc["packages"][pin::package_key(key)].get("peerDependencies");
        peers.retain(|name, _| {
            name == "patch_hash" || own.is_some_and(|own| own.get(name).is_some())
        });
        let mut versions_only = BTreeSet::new();
        for (name, values) in &found {
            let first = values.first()?;
            let value = if values.len() == 1 {
                first.clone()
            } else if flat
                && values.iter().all(|value| {
                    pin::package_key(value) == pin::package_key(first)
                        && hash_in_key(value) == hash_in_key(first)
                })
            {
                versions_only.insert((*name).into());
                pin::package_key(first).to_owned()
            } else {
                alternatives.insert((*name).into(), values.clone());
                continue;
            };
            peers.insert((*name).into(), value);
        }
        return Some(Bindings {
            peers: peers.into(),
            hash: style,
            flat,
            linked_peer_encoding,
            versions_only,
        });
    }
    search::recover(doc, key, &direct, &found, &hash, style, &graph.parallelism)
}

fn transitive_aliases(
    doc: &Value,
    key: &str,
    name: &str,
    context: &[(&str, &str)],
) -> BTreeSet<String> {
    let mut pending = vec![key.to_owned()];
    let mut seen = HashSet::new();
    let mut result = BTreeSet::new();
    while let Some(key) = pending.pop() {
        if !seen.insert(key.clone()) {
            continue;
        }
        let Some(record) = doc.get("snapshots").and_then(|v| v.get(&key)) else {
            continue;
        };
        if let Some(value) = ["dependencies", "optionalDependencies"]
            .into_iter()
            .find_map(|group| record.get(group)?.get(name)?.as_str())
        {
            let (actual, reference) = canonical_binding(name, value);
            if actual != name
                && doc["packages"][pin::package_key(&key)]["peerDependencies"]
                    .get(name)
                    .is_some()
                && context.iter().any(|(n, v)| {
                    *n == actual && (*v == reference || *v == pin::package_key(reference))
                })
            {
                result.insert(value.into());
            }
            continue;
        }
        if record["transitivePeerDependencies"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(name)))
        {
            for group in ["dependencies", "optionalDependencies"] {
                for (name, value) in graph::section(record, group).into_iter().flatten() {
                    if let Some(value) = value.as_str() {
                        pending.push(graph::snapshot_key(
                            name,
                            value,
                            graph::section(doc, "snapshots"),
                        ));
                    }
                }
            }
        }
    }
    result
}
