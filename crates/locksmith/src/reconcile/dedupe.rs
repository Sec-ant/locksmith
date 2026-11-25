//! Reconcile ordinary versions brought together by an autoDedupe workspace merge.
use super::PUBLISHED;
use std::collections::{BTreeMap, HashMap, HashSet};

use node_semver::Version;
use serde_json::json;

use super::{GraphSources, accepts, published, reference};
use crate::{LockfileError, Value, graph, manifest};

struct Edge<'a> {
    parent: &'a str,
    group: &'static str,
    name: &'a str,
}

pub(super) fn reconcile(
    output: &mut Value,
    sources: &GraphSources<'_>,
) -> Result<(), LockfileError> {
    if !sources.configuration.auto_dedupe {
        return Ok(());
    }
    let Some(snapshots) = graph::section(output, "snapshots") else {
        return Ok(());
    };
    let mut versions: BTreeMap<&str, Vec<(Version, &str)>> = BTreeMap::new();
    for key in snapshots.keys() {
        if key.contains('(')
            || !output["packages"][key]["resolution"]["integrity"].is_string()
            || output["packages"][key].get("peerDependencies").is_some()
        {
            continue;
        }
        if let Some((name, version)) = key.rsplit_once('@')
            && let Ok(parsed) = version.parse()
        {
            versions.entry(name).or_default().push((parsed, key));
        }
    }
    let mut candidates = BTreeMap::new();
    for records in versions.values_mut().filter(|records| records.len() > 1) {
        records.sort_unstable_by(|left, right| right.0.cmp(&left.0));
        for (i, (_, old)) in records.iter().enumerate().skip(1) {
            let newer: Vec<_> = records[..i]
                .iter()
                .map(|(_, key)| *key)
                .filter(|new| {
                    sources.inputs[..PUBLISHED]
                        .iter()
                        .map(|input| input.document)
                        .all(|doc| {
                            doc["packages"].get(old).is_none() || doc["packages"].get(new).is_none()
                        })
                })
                .collect();
            if !newer.is_empty() {
                candidates.insert(*old, newer);
            }
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let mut protected = HashSet::new();
    for (_, record) in graph::section(output, "importers").into_iter().flatten() {
        for group in manifest::GROUPS {
            for (name, dep) in graph::section(record, group).into_iter().flatten() {
                if let Some(pin) = dep["version"].as_str() {
                    let key = graph::snapshot_key(name, pin, Some(snapshots));
                    if candidates.contains_key(key.as_str()) {
                        protected.insert(key);
                    }
                }
            }
        }
    }
    let mut consumers: HashMap<String, Vec<Edge<'_>>> = HashMap::new();
    for (parent, body) in snapshots {
        for group in ["dependencies", "optionalDependencies"] {
            for (name, value) in graph::section(body, group).into_iter().flatten() {
                let Some(pin) = value.as_str() else { continue };
                let key = graph::snapshot_key(name, pin, Some(snapshots));
                if !candidates.contains_key(key.as_str()) || protected.contains(&key) {
                    continue;
                }
                let pkg = &output["packages"][graph::package_key(parent)];
                if pkg["peerDependencies"].get(name).is_some()
                    || body["transitivePeerDependencies"]
                        .as_array()
                        .is_some_and(|peers| peers.iter().any(|p| p == name))
                {
                    protected.insert(key);
                } else {
                    consumers.entry(key).or_default().push(Edge {
                        parent,
                        group,
                        name,
                    });
                }
            }
        }
    }
    let mut edits = Vec::new();
    for (old, alternatives) in candidates {
        if protected.contains(old) {
            continue;
        }
        let Some(edges) = consumers.get(old) else {
            continue;
        };
        for target in alternatives {
            let mut compatible = true;
            for edge in edges {
                let pin = reference(edge.name, target);
                if !allows(output, sources, edge, &pin)? {
                    compatible = false;
                    break;
                }
            }
            if compatible {
                for edge in edges {
                    edits.push((
                        edge.parent.to_owned(),
                        edge.group,
                        edge.name.to_owned(),
                        reference(edge.name, target),
                    ));
                }
                break;
            }
        }
    }
    for (parent, group, name, pin) in edits {
        output["snapshots"][parent][group][name] = pin.into();
    }
    Ok(())
}

fn allows(
    output: &Value,
    sources: &GraphSources<'_>,
    edge: &Edge<'_>,
    pin: &str,
) -> Result<bool, LockfileError> {
    let parent = graph::package_key(edge.parent);
    let pkg = &output["packages"][parent];
    let witnessed = sources.inputs[..PUBLISHED]
        .iter()
        .map(|input| input.document)
        .enumerate()
        .any(|(side, doc)| {
            sources.inputs[side].unchanged_rules
                && sources.inputs[side].compatibility.unchanged(parent)
                && doc["packages"][parent]["resolution"] == pkg["resolution"]
                && doc["snapshots"][edge.parent][edge.group][edge.name] == pin
        });
    if witnessed {
        return Ok(true);
    }
    let Some((name, version)) = parent.rsplit_once('@') else {
        return Ok(false);
    };
    if version.parse::<Version>().is_err() || !pkg["resolution"]["integrity"].is_string() {
        return Ok(false);
    }
    let Some(range) = published::original(
        sources,
        pkg,
        parent,
        edge.group,
        edge.name,
        "autoDedupe needs the original range to reconcile versions from different branches",
    )?
    else {
        return Ok(false);
    };
    let parent_manifest = json!({"name":name,"version":version});
    let Some(range) = sources
        .final_rules
        .apply(&parent_manifest, ".", edge.name, &range)?
    else {
        return Ok(false);
    };
    Ok(accepts(edge.name, &range, pin, output))
}
