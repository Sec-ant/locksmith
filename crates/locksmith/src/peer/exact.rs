use super::{Merger, explicit_bindings, patch, satisfies_peer};
use crate::{
    Value, WorkspaceConfig,
    graph::{self, dependency, transitive},
};

impl Merger<'_> {
    /// An exact override can repair an ordinary edge when all consumer scopes
    /// witness every concrete peer of one already locked artifact.
    pub(crate) fn exact_override<const N: usize>(
        &self,
        parents: [&str; N],
        config: &WorkspaceConfig,
        name: &str,
        spec: &str,
    ) -> Option<String> {
        let (target, version) = crate::reconcile::alias(spec).unwrap_or((name, spec));
        let version = version.parse::<node_semver::Version>().ok()?.to_string();
        let release = format!("{target}@{version}");
        let donor_side = parents.len().checked_sub(1)?;
        let donor = self.documents[donor_side]?;
        let artifact = donor["packages"].get(&release)?.get("resolution")?;
        if !artifact["integrity"].is_string()
            || self.documents[..donor_side].iter().flatten().any(|doc| {
                doc["packages"]
                    .get(&release)
                    .is_some_and(|v| v.get("resolution") != Some(artifact))
            })
            || config
                .patches
                .iter()
                .flatten()
                .any(|(selector, _)| crate::manifest::parent_matches(selector, target, &version))
        {
            return None;
        }
        let scopes = parents.map(explicit_bindings);
        let mut chosen = None;
        for key in self.locked_instances(donor_side, &release) {
            let body = &donor["snapshots"][key];
            let bindings = explicit_bindings(key)?;
            if patch(key).is_some() || transitive(body).next().is_some() {
                return None;
            }
            let declared = graph::section(&donor["packages"][&release], "peerDependencies");
            if declared.map_or(0, |d| d.len()) != bindings.len() {
                return None;
            }
            let witnessed = bindings.iter().all(|(&peer_name, &token)| {
                let Some((_, pin)) = dependency(body, peer_name) else {
                    return false;
                };
                let fits = declared
                    .and_then(|d| d.get(peer_name))
                    .and_then(Value::as_str)
                    .zip(graph::package_key(pin).parse::<node_semver::Version>().ok())
                    .is_some_and(|(range, v)| satisfies_peer(&v, range));
                if !fits || !same_binding(token, pin, config.resolution.lock.dedupe_peers) {
                    return false;
                }
                let provider = format!("{peer_name}@{pin}");
                parents.iter().enumerate().all(|(side, parent)| {
                    let Some(doc) = self.documents[side] else {
                        return false;
                    };
                    if doc["snapshots"].get(&provider).is_none() {
                        return false;
                    }
                    if let Some((_, local)) = dependency(&doc["snapshots"][*parent], peer_name) {
                        return local == pin;
                    }
                    let Some(exposed) = scopes[side].as_ref().and_then(|b| b.get(peer_name)) else {
                        return false;
                    };
                    *exposed == pin
                        || same_binding(exposed, pin, config.resolution.lock.dedupe_peers)
                            && self
                                .locked_instances(side, graph::package_key(&provider))
                                .all(|k| k == provider)
                })
            });
            if witnessed {
                if chosen.is_some_and(|k| k != key) {
                    return None;
                }
                chosen = Some(key);
            }
        }
        chosen.map(|k| crate::reconcile::reference(name, k))
    }
}

pub(crate) fn same_binding(a: &str, b: &str, flat: bool) -> bool {
    a == b
        || flat
            && graph::package_key(a) == graph::package_key(b)
            && (!a.contains('(') || !b.contains('('))
}
