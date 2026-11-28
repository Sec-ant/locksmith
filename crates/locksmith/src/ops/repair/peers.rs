//! Keep consumers bound to the providers selected in the repaired importer.
use super::Evidence;
use crate::{
    LockfileError, Value, model::error::missing, model::graph, model::pin, model::schema, peer,
    resolve::peer_names,
};
use std::collections::BTreeMap;

fn scope(doc: &Value, id: &str, root_peers: bool) -> BTreeMap<String, String> {
    let mut providers = BTreeMap::new();
    for importer in (root_peers && id != ".")
        .then_some(".")
        .into_iter()
        .chain([id])
    {
        for group in schema::DEPENDENCY_GROUPS {
            for (name, dep) in graph::section(&doc["importers"][importer], group)
                .into_iter()
                .flatten()
            {
                if let Some(pin) = dep["version"].as_str() {
                    providers.insert(name.clone(), peer::root_provider(importer, pin));
                }
            }
        }
    }
    providers
}

pub(super) fn affected(
    doc: &Value,
    evidence: &mut Evidence<'_>,
) -> Result<Vec<(String, &'static str, String, String)>, LockfileError> {
    let before = evidence.lock.project().expect("parsed project");
    let root_peers = evidence
        .context
        .configuration
        .resolution
        .workspace_root_peers;
    let mut edits = Vec::new();
    for (id, importer) in graph::section(doc, "importers").into_iter().flatten() {
        if importer == &before["importers"][id]
            && (!root_peers || doc["importers"]["."] == before["importers"]["."])
        {
            continue;
        }
        let current = scope(doc, id, root_peers);
        let old = scope(before, id, root_peers);
        if current == old {
            continue;
        }
        for group in schema::DEPENDENCY_GROUPS {
            for (name, dep) in graph::section(importer, group).into_iter().flatten() {
                let Some(pin) = dep["version"].as_str().filter(|v| !v.starts_with("link:")) else {
                    continue;
                };
                let identity = graph::snapshot_key(name, pin, graph::section(doc, "snapshots"));
                let names = peer_names(doc, &identity);
                if names.is_empty()
                    || (old.get(name) == current.get(name)
                        && names.iter().all(|p| old.get(p) == current.get(p)))
                {
                    continue;
                }
                let donor = evidence.resolved()?;
                let candidate = donor["importers"][id][group][name]["version"]
                    .as_str()
                    .ok_or_else(|| {
                        missing(
                            &["importers", id, group, name],
                            "no witnessed peer instance",
                        )
                    })?;
                let other = scope(donor, id, root_peers);
                if pin::package_key(candidate) != pin::package_key(pin)
                    || names.iter().any(|p| other.get(p) != current.get(p))
                {
                    return Err(missing(
                        &["importers", id, group, name],
                        "no witnessed instance for the retained peer providers",
                    ));
                }
                if pin != candidate {
                    edits.push((id.clone(), group, name.clone(), candidate.to_owned()));
                }
            }
        }
    }
    Ok(edits)
}
