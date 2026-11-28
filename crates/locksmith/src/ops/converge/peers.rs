use std::collections::{BTreeMap, BTreeSet};

use serde_json::Map;

use crate::{
    LockfileError, Value, WorkspaceConfig, model::graph, model::patch, model::pin, model::schema,
    peer, resolve,
};

type Providers = BTreeMap<String, String>;

fn providers(importers: &Value, id: &str, inherit: bool) -> Providers {
    let scope = peer::importer_providers(importers, id, inherit);
    match peer::canonical_providers(&scope) {
        std::borrow::Cow::Borrowed(_) => scope,
        std::borrow::Cow::Owned(canonical) => canonical,
    }
}

struct Pin {
    group: &'static str,
    name: String,
    side: usize,
    reference: String,
}

struct Bound {
    side: usize,
    witness: String,
    reference: String,
}

/// Rebind an importer against the providers it will actually use. The locked
/// source of each root supplies its ordinary dependencies and untouched peers.
pub(super) fn bind(
    docs: [&Value; 2],
    importers: &mut Value,
    retained: &mut Map<String, Value>,
    adopted: &mut Map<String, Value>,
    config: Option<&WorkspaceConfig>,
) -> Result<BTreeMap<String, Value>, LockfileError> {
    let inherit = config.is_none_or(|c| c.resolution.workspace_root_peers);
    let mut peers = peer::Merger::new([Some(docs[0]), Some(docs[1]), None]);
    if let Some(config) = config {
        peers.configure_context(
            [0, 1, 2, 3],
            config.resolution.lock.dedupe_peers,
            config.linked_peer_encoding,
        );
    }
    // Resolve the root first so workspace members inherit its final providers.
    let mut ids: BTreeSet<_> = importers
        .as_object()
        .expect("importer map")
        .keys()
        .cloned()
        .collect();
    for id in ids.take(".").into_iter().chain(ids) {
        if docs
            .iter()
            .zip([&*retained, &*adopted])
            .all(|(doc, edges)| {
                edges[&id].as_object().expect("selected edges").is_empty()
                    || (doc["importers"][&id] == importers[&id]
                        && (!inherit || doc["importers"]["."] == importers["."]))
            })
        {
            continue;
        }
        let previous = docs.map(|d| providers(&d["importers"], &id, inherit));
        let mut roots = Vec::new();
        for (side, source) in [&*retained, &*adopted].into_iter().enumerate() {
            for group in schema::DEPENDENCY_GROUPS {
                for (name, dep) in graph::section(&source[&id], group).into_iter().flatten() {
                    if let Some(reference) =
                        dep["version"].as_str().filter(|r| !r.starts_with("link:"))
                    {
                        roots.push(Pin {
                            group,
                            name: name.clone(),
                            side,
                            reference: reference.into(),
                        });
                    }
                }
            }
        }
        for pass in 0..=roots.len() {
            let scope = providers(importers, &id, inherit);
            let changed = previous.each_ref().map(|before| {
                before
                    .keys()
                    .chain(scope.keys())
                    .filter(|n| before.get(*n) != scope.get(*n))
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>()
            });
            let mut next = Vec::new();
            let mut settled = true;
            let mut unresolved = None;
            peers.begin_pass();
            for pin in &roots {
                let bound = match select(
                    &mut peers,
                    docs,
                    &id,
                    pin,
                    &previous,
                    &scope,
                    &changed[pin.side],
                ) {
                    Ok(bound) => bound,
                    Err(error) => {
                        unresolved.get_or_insert(error);
                        continue;
                    }
                };
                let reference = bound.as_ref().map_or(&pin.reference, |b| &b.reference);
                let entry = &mut importers[&id][pin.group][&pin.name]["version"];
                if entry.as_str() != Some(reference) {
                    *entry = Value::from(reference.clone());
                    settled = false;
                }
                if let Some(bound) = bound {
                    next.push((pin, bound));
                }
            }
            peers.finish_pass(settled);
            if settled {
                if let Some(error) = unresolved {
                    return Err(error);
                }
                for (pin, bound) in next {
                    if pin.side != bound.side {
                        let (from, to) = if pin.side == 0 {
                            (&mut *retained, &mut *adopted)
                        } else {
                            (&mut *adopted, &mut *retained)
                        };
                        let mut dep = from[&id][pin.group]
                            .as_object_mut()
                            .unwrap()
                            .remove(&pin.name)
                            .unwrap();
                        dep["version"] = Value::from(bound.witness);
                        to[&id][pin.group][&pin.name] = dep;
                    }
                }
                break;
            }
            if pass == roots.len() {
                return Err(crate::model::error::missing(
                    &["importers", &id],
                    "peer bindings do not reach a stable context",
                ));
            }
        }
    }
    Ok(peers.into_generated())
}

fn select(
    peers: &mut peer::Merger<'_>,
    docs: [&Value; 2],
    id: &str,
    pin: &Pin,
    previous: &[Providers; 2],
    scope: &Providers,
    changed: &BTreeSet<&str>,
) -> Result<Option<Bound>, LockfileError> {
    if changed.is_empty() {
        return Ok(None);
    }
    let side = pin.side;
    let other = 1 - side;
    let name = pin.name.as_str();
    let key = graph::snapshot_key(
        name,
        &pin.reference,
        graph::section(docs[side], "snapshots"),
    );
    let mut names = resolve::peer_names(docs[side], &key);
    if let Some(explicit) = peer::explicit_bindings(&key) {
        names.extend(
            explicit
                .keys()
                .filter(|n| **n != "patch_hash")
                .map(|n| (*n).to_owned()),
        );
    }
    if !names.iter().any(|n| changed.contains(n.as_str())) {
        return Ok(None);
    }
    let unresolved = || {
        crate::model::error::missing(
            &["importers", id, pin.group, name],
            "cannot determine peer bindings for the selected providers",
        )
    };
    let original = peers
        .original_context(side, &key, &previous[side])
        .ok_or_else(unresolved)?;
    let alternate = super::importer_dependency(docs[other], id, name)
        .and_then(|(_, dep)| dep["version"].as_str())
        .map(|r| {
            (
                r,
                graph::snapshot_key(name, r, graph::section(docs[other], "snapshots")),
            )
        })
        .filter(|(_, k)| {
            pin::package_key(k) == pin::package_key(&key)
                && patch::hash_in_key(k) == patch::hash_in_key(&key)
                && docs[side]["packages"][pin::package_key(&key)]
                    == docs[other]["packages"][pin::package_key(k)]
        })
        .and_then(|(r, k)| {
            peers
                .original_context(other, &k, &previous[other])
                .map(|bindings| (r, k, bindings))
        });
    let mut desired = (*original).clone();
    for n in names.iter().filter(|n| changed.contains(n.as_str())) {
        if let Some((_, _, bindings)) = &alternate
            && scope.get(n) == previous[other].get(n)
        {
            if let Some(value) = bindings.get(n) {
                desired.insert(n.clone(), value.clone());
            } else {
                desired.remove(n);
            }
        } else if let Some(provider) = scope.get(n)
            && original.contains_key(n)
        {
            desired.insert(n.clone(), provider.clone());
        } else if scope.contains_key(n) || original.contains_key(n) {
            return Err(unresolved());
        }
    }
    if desired == *original {
        return Ok(None);
    }
    if let Some((r, _, bindings)) = &alternate
        && **bindings == desired
    {
        return Ok(Some(Bound {
            side: other,
            witness: (*r).into(),
            reference: (*r).into(),
        }));
    }
    // Rebinding needs a witnessed set of peers. The other lock can supply
    // that set when this edit adds or removes an optional provider.
    let (side, key, witness) = if original.keys().eq(desired.keys()) {
        (side, key, pin.reference.as_str())
    } else {
        let (r, k, _) = alternate
            .filter(|(_, _, bindings)| bindings.keys().eq(desired.keys()))
            .ok_or_else(unresolved)?;
        (other, k, r)
    };
    let rebound = peers
        .rebind(side, &key, &desired, &previous[side])
        .ok_or_else(unresolved)?;
    Ok(Some(Bound {
        side,
        witness: witness.into(),
        reference: pin::reference(name, &rebound),
    }))
}
