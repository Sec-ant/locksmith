//! Merge three lockfile snapshots without a final workspace context.
use std::collections::{BTreeMap, BTreeSet};

use crate::{Value, graph, patch::hash_in_key, pin, pin::reference};

use super::{Merger, bindings::bindings, identity::encode};

enum ThreeWay<T> {
    Agreed(T),
    Conflict,
}

// For optional fields an agreed None is a deletion, distinct from conflict.
fn choose<T: PartialEq>(base: T, ours: T, theirs: T) -> ThreeWay<T> {
    if ours == theirs || theirs == base {
        ThreeWay::Agreed(ours)
    } else if ours == base {
        ThreeWay::Agreed(theirs)
    } else {
        ThreeWay::Conflict
    }
}

impl<'a> Merger<'a> {
    pub fn dependency(
        &mut self,
        name: &str,
        b: Option<&Value>,
        o: Option<&Value>,
        t: Option<&Value>,
    ) -> Option<Value> {
        let (b, o, t) = (b?, o?, t?);
        let ThreeWay::Agreed(Some(specifier)) =
            choose(b.get("specifier"), o.get("specifier"), t.get("specifier"))
        else {
            return None;
        };
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
                        pin::package_key(key),
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
        if let ThreeWay::Agreed(value) = choose(base, ours, theirs) {
            return Some(value.to_owned());
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
        let packages: Vec<_> = keys.iter().map(|k| pin::package_key(k)).collect();
        // A new package release can have different dependency declarations.
        // Rebind that release's graph without copying old-release dependencies
        // or substituting a different consumer's regular resolutions.
        let ThreeWay::Agreed(package) = choose(packages[0], packages[1], packages[2]) else {
            return None;
        };
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
                let value = if let ThreeWay::Agreed(v) = choose(values[0], values[1], values[2]) {
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
            || keys.iter().any(|k| hash_in_key(k) != hash_in_key(&keys[0]))
        {
            let (_, peers) = desired?;
            let mut rebound = None;
            for (side, source) in packages.iter().enumerate() {
                if *source == package
                    && hash_in_key(&keys[side]) == peers.get("patch_hash").map(String::as_str)
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

    // Snapshot maps merge by dependency edge; flags remain ordinary three-way
    // values. Each recursive reference is memoized across workspace consumers.
    pub fn value(
        &mut self,
        field: Option<&str>,
        b: Option<&Value>,
        o: Option<&Value>,
        t: Option<&Value>,
    ) -> Option<Option<Value>> {
        if let ThreeWay::Agreed(v) = choose(b, o, t) {
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
                    if let ThreeWay::Agreed(v) = choose(values[0], values[1], values[2]) {
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
