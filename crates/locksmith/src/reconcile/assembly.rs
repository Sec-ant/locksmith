//! Follow reachable dependency edges and install each repaired snapshot with its source metadata.
use super::repair::repair_snapshot;
use super::{
    GraphSources, PeerChanges, RepairedSnapshot, authorized_patch, expand, missing, peer_names,
};
use crate::{LockfileError, Value, graph, manifest, peer};
use serde_json::json;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn assemble(
    output: &mut Value,
    sources: &GraphSources<'_>,
    peers: &mut peer::Merger<'_>,
    configured_sources: &BTreeMap<String, usize>,
    source_order: [usize; 4],
    preferences: &super::preference::Preferences,
    alternative: usize,
) -> Result<bool, LockfileError> {
    let docs = sources.docs;
    output["snapshots"] = json!({});
    output["packages"] = json!({});
    let mut pending = Vec::new();
    for (_, record) in graph::section(output, "importers").into_iter().flatten() {
        for group in manifest::GROUPS {
            for (name, dep) in graph::section(record, group).into_iter().flatten() {
                let value = dep["version"].as_str().expect("selected version");
                if !value.starts_with("link:") {
                    pending.push(peers.snapshot_key(name, value));
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut extended = false;
    loop {
        let mut new_peers = PeerChanges::default();
        while let Some(key) = pending.pop() {
            if !seen.insert(key.clone()) || output["snapshots"].get(&key).is_some() {
                continue;
            }
            if !authorized_patch(&key, output, sources.docs) {
                return Err(missing(
                    &["snapshots", &key],
                    "instance does not use the final patch",
                ));
            }
            let package_key = graph::package_key(&key);
            let mut selected = None;
            let mut reconstructed = None;
            let mut order = source_order;
            if preferences.snapshot(&key) {
                order.sort_by_key(|side| *side != alternative);
            }
            for side in configured_sources
                .get(&key)
                .copied()
                .into_iter()
                .chain(order)
            {
                if !sources.compatibility[side].accepts(&key) {
                    continue;
                }
                if let Some(body) = docs[side].get("snapshots").and_then(|v| v.get(&key)) {
                    if sources.compatibility[side].unchanged(&key) {
                        selected = Some((side, Cow::Borrowed(body)));
                        break;
                    }
                    reconstructed.get_or_insert((side, Cow::Borrowed(body)));
                }
            }
            if let Some(side) = peers.origin_side(&key)
                && !sources.compatibility[side].unchanged(&key)
            {
                if selected.is_none() && !sources.compatibility[side].accepts(&key) {
                    return Err(missing(
                        &["snapshots", &key],
                        "peer instance uses a different resolution configuration",
                    ));
                }
                // The same identity may have an input record produced under
                // the final configuration. Its body and provenance replace
                // the generated record together.
                if selected.is_some() {
                    peers.discard_generated(&key);
                }
            }
            let selected = selected.or(reconstructed);
            if let Some((side, _)) = &selected
                && peers.origin_side(&key) == Some(*side)
                && peers.origin_key(&key) != key
            {
                peers.discard_generated(&key);
            }
            let (side, body) = if let Some((side, body)) = peers.rebound_snapshot(&key) {
                (side, Cow::Owned(body.clone()))
            } else {
                selected.ok_or_else(|| {
                    if key == package_key
                        && let Some((name, version)) = package_key.rsplit_once('@')
                    {
                        return super::published::need_version(
                            &["snapshots", &key],
                            name,
                            version,
                            sources.configuration,
                            "no compatible instance in the supplied graphs".into(),
                        );
                    }
                    missing(
                        &["snapshots", &key],
                        "no compatible instance in the supplied graphs",
                    )
                })?
            };
            let package = docs[side]
                .get("packages")
                .and_then(|v| v.get(package_key))
                .ok_or_else(|| missing(&["packages", package_key], "missing package metadata"))?;
            let mut repaired = repair_snapshot(&key, side, &body, package, output, sources, peers)?;
            if side == super::PUBLISHED {
                let names = peer_names(docs[side], &key);
                if !names.is_empty() {
                    repaired
                        .peers
                        .required
                        .entry(key.clone())
                        .or_default()
                        .extend(names);
                }
                repaired.extended = true;
            }
            extended |= repaired.extended;
            let RepairedSnapshot {
                body,
                package,
                peers: additions,
                ..
            } = repaired;
            for (child, names) in additions.required {
                new_peers.required.entry(child).or_default().extend(names);
            }
            for (child, names) in additions.affected {
                new_peers.affected.entry(child).or_default().extend(names);
            }
            for (child, names) in additions.unresolved {
                new_peers.unresolved.entry(child).or_default().extend(names);
            }
            for (parent, names) in additions.supplied {
                new_peers.supplied.entry(parent).or_default().extend(names);
            }
            for group in ["dependencies", "optionalDependencies"] {
                for (name, value) in graph::section(&body, group).into_iter().flatten() {
                    let value = value.as_str().ok_or_else(|| {
                        missing(&["snapshots", &key, name], "expected dependency reference")
                    })?;
                    if !value.starts_with("link:") {
                        pending.push(peers.snapshot_key(name, value));
                    }
                }
            }
            output["snapshots"][&key] = body;
            output["packages"][package_key] = package;
        }
        if new_peers.required.is_empty()
            && new_peers.affected.is_empty()
            && new_peers.supplied.is_empty()
        {
            break;
        }
        pending = expand::rebuild(output, peers, new_peers, sources.configuration, |key| {
            configured_sources.get(key).copied().or_else(|| {
                source_order.iter().copied().find(|side| {
                    sources.compatibility[*side].accepts(key)
                        && docs[*side]
                            .get("snapshots")
                            .and_then(|v| v.get(key))
                            .is_some()
                })
            })
        })?;
        if pending.is_empty() {
            break;
        }
    }
    Ok(extended)
}
