//! Project a workspace rule change onto the baseline, using installed instances
//! as witnesses. Only paths to releases matched by a changed rule are paired.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::{
    DocumentKind, Lockfile, LockfileError, Value, WorkspaceConfig, config, graph, manifest, peer,
    reconcile,
};
use reconcile::missing;

const EDGES: [&str; 2] = ["dependencies", "optionalDependencies"];
mod bindings;
use bindings::{Binding, Contributions, Edits, Interface};

#[derive(Clone)]
struct Instance {
    key: String,
    peers: Edits,
}

fn edges(body: &Value) -> impl Iterator<Item = (&'static str, &String, &Value)> {
    EDGES.into_iter().flat_map(|g| {
        graph::section(body, g)
            .into_iter()
            .flatten()
            .map(move |(n, v)| (g, n, v))
    })
}

fn dependency<'a>(body: &'a Value, name: &str) -> Option<(&'static str, &'a str)> {
    EDGES
        .into_iter()
        .find_map(|group| Some((group, body[group].get(name)?.as_str()?)))
}

fn transitive(body: &Value) -> impl Iterator<Item = &str> {
    body["transitivePeerDependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

fn checked_checksum(doc: &Value, cfg: &WorkspaceConfig) -> Result<(), LockfileError> {
    let extensions = cfg
        .resolution
        .package_extensions
        .as_ref()
        .filter(|v| v.as_object().is_none_or(|m| !m.is_empty()));
    let hash = doc.get("packageExtensionsChecksum");
    let valid = match (extensions, hash) {
        (None, None) => true,
        (Some(v), Some(h)) => h.as_str().is_some_and(|s| {
            crate::checksum::extensions(v, v, s, cfg.extension_checksum).as_deref() == Some(s)
        }),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(missing(
            &["packageExtensionsChecksum"],
            "lockfile does not match its workspace package extensions",
        ))
    }
}

pub(super) fn apply(
    baseline: &Lockfile,
    candidate: &Lockfile,
    before: &WorkspaceConfig,
    after: &WorkspaceConfig,
    manifests: &[super::ManifestChange],
) -> Result<Option<Lockfile>, LockfileError> {
    fn rules(c: &WorkspaceConfig) -> Option<&serde_json::Map<String, Value>> {
        c.resolution
            .package_extensions
            .as_ref()
            .and_then(Value::as_object)
    }
    if before == after
        && rules(before)
            .into_iter()
            .flatten()
            .eq(rules(after).into_iter().flatten())
    {
        return Ok(None);
    }
    let mut expected = before.clone();
    expected
        .resolution
        .package_extensions
        .clone_from(&after.resolution.package_extensions);
    if expected != *after {
        return Err(missing(
            &["configuration"],
            "workspace resolution settings changed outside packageExtensions",
        ));
    }
    let base = baseline
        .project()
        .ok_or_else(|| missing(&[], "missing baseline project lockfile"))?;
    let donor = candidate
        .project()
        .ok_or_else(|| missing(&[], "missing installed project lockfile"))?;
    checked_checksum(base, before)?;
    checked_checksum(donor, after)?;
    let policy = config::ResolutionCompatibility::new(&before.resolution, &after.resolution, base);
    let affected = super::affected_by(base, |key| !policy.unchanged(key));
    let mut output = base.clone();
    if let Some(checksum) = donor.get("packageExtensionsChecksum") {
        output["packageExtensionsChecksum"] = checksum.clone();
    } else {
        output
            .as_object_mut()
            .expect("project mapping")
            .remove("packageExtensionsChecksum");
    }
    if !affected.is_empty() {
        let mut walk = Projector {
            docs: [base, donor],
            policy,
            config: after,
            overrides: manifest::Overrides::new(donor, after.override_tie_break),
            affected,
            peers: peer::Merger::new([Some(base), Some(donor), None]),
            active: HashSet::new(),
            memo: HashMap::new(),
            records: BTreeMap::new(),
            new_children: BTreeSet::new(),
        };
        walk.peers.configure_context(
            [0, 1, 2, 3],
            after.resolution.lock.dedupe_peers,
            after.linked_peer_encoding,
        );
        for (id, root) in graph::section(base, "importers").into_iter().flatten() {
            for group in manifest::GROUPS {
                for (name, dep) in graph::section(root, group).into_iter().flatten() {
                    let Some(pin) = dep["version"].as_str() else {
                        return Err(missing(&["importers", id, name], "missing baseline pin"));
                    };
                    let old = graph::snapshot_key(name, pin, graph::section(base, "snapshots"));
                    if !walk.affected.contains(&old) {
                        continue;
                    }
                    let current = &donor["importers"][id][group][name];
                    if current.is_null() {
                        let removed =
                            manifests
                                .iter()
                                .find(|m| m.importer == id)
                                .is_some_and(|m| {
                                    m.after.is_none_or(|v| {
                                        v.get(group).and_then(|deps| deps.get(name)).is_none()
                                            && v.get("peerDependencies")
                                                .and_then(|deps| deps.get(name))
                                                .is_none()
                                    })
                                });
                        if removed {
                            continue;
                        }
                        return Err(missing(
                            &["importers", id, group, name],
                            "installed lock is missing the affected dependency",
                        ));
                    }
                    let next = walk.witness(name, &old, current.get("version"))?;
                    let selected = walk.visit(&old, &next)?;
                    output["importers"][id][group][name]["version"] =
                        reconcile::reference(name, &selected.key).into();
                    if selected.peers.is_empty() {
                        continue;
                    }
                    let providers = peer::importer_providers(
                        &base["importers"],
                        id,
                        after.resolution.workspace_root_peers,
                    );
                    for (peer, (_, bound)) in &selected.peers {
                        if let Binding::Bound(bound) = bound
                            && let Some(provider) = providers.get(peer)
                            && !walk.same_binding(bound, provider)
                        {
                            return Err(missing(
                                &["importers", id, name, peer],
                                "installed peer does not match the retained provider",
                            ));
                        }
                    }
                }
            }
        }
        walk.materialize(&mut output)?;
        graph::prune_scoped(&mut output, DocumentKind::Project, base, Some(after))?;
    }
    Ok(Some(baseline.converged_project(output, candidate)))
}

struct SnapshotEdit {
    body: Value,
    touched: BTreeSet<String>,
    peers: Contributions,
}

impl SnapshotEdit {
    fn peer_change(&mut self, child: &str, peer: &str, before: &Binding, after: &Binding) {
        if before != after {
            self.peers
                .entry(peer.into())
                .or_default()
                .insert(child.into(), after.clone());
        }
    }
}

struct Projector<'a> {
    docs: [&'a Value; 2],
    policy: config::ResolutionCompatibility<'a>,
    config: &'a WorkspaceConfig,
    overrides: manifest::Overrides<'a>,
    affected: BTreeSet<String>,
    peers: peer::Merger<'a>,
    active: HashSet<(String, String)>,
    memo: HashMap<(String, String), Instance>,
    records: BTreeMap<String, Value>,
    new_children: BTreeSet<String>,
}

impl Projector<'_> {
    fn witness(&self, name: &str, old: &str, pin: Option<&Value>) -> Result<String, LockfileError> {
        let snapshots = graph::section(self.docs[1], "snapshots");
        if let Some(next) = pin
            .and_then(Value::as_str)
            .map(|v| graph::snapshot_key(name, v, snapshots))
            && graph::package_key(old) == graph::package_key(&next)
            && peer::patch(old) == peer::patch(&next)
            && snapshots.is_some_and(|m| m.contains_key(&next))
        {
            return Ok(next);
        }
        if snapshots.is_some_and(|m| m.contains_key(old)) {
            return Ok(old.into());
        }
        Err(missing(
            &["snapshots", old],
            "installed lock has no corresponding instance of the baseline release",
        ))
    }

    fn visit(&mut self, old: &str, next: &str) -> Result<Instance, LockfileError> {
        let pair = (old.to_owned(), next.to_owned());
        if let Some(result) = self.memo.get(&pair) {
            return Ok(result.clone());
        }
        if !self.active.insert(pair.clone()) {
            return Err(missing(
                &["snapshots", old],
                "workspace edit crosses a peer cycle without a unique projection",
            ));
        }
        let [base, donor] = self.docs;
        let pkg = graph::package_key(old);
        if base["packages"][pkg].get("resolution") != donor["packages"][pkg].get("resolution")
            || !base["packages"][pkg]["resolution"]["integrity"].is_string()
        {
            return Err(missing(
                &["packages", pkg],
                "workspace edit requires the same verified package artifact",
            ));
        }
        for field in ["peerDependencies", "peerDependenciesMeta"] {
            if base["packages"][pkg].get(field) != donor["packages"][pkg].get(field) {
                return Err(missing(
                    &["packages", pkg, field],
                    "workspace edit changes the declared peer requirements",
                ));
            }
        }
        let previous = &base["snapshots"][old];
        let installed = &donor["snapshots"][next];
        let mut edit = SnapshotEdit {
            body: previous.clone(),
            touched: BTreeSet::new(),
            peers: Contributions::new(),
        };
        self.apply_rules(old, next, &mut edit)?;
        for (group, name, pin) in edges(previous) {
            if edit.touched.contains(name) {
                continue;
            }
            let pin = pin.as_str().ok_or_else(|| {
                missing(&["snapshots", old, name], "invalid dependency reference")
            })?;
            let child = graph::snapshot_key(name, pin, graph::section(base, "snapshots"));
            if !self.affected.contains(&child) {
                continue;
            }
            let witness = self.witness(name, &child, installed[group].get(name))?;
            let projected = self.visit(&child, &witness)?;
            let (dest, _) = dependency(&edit.body, name).expect("retained dependency");
            edit.body[dest][name] = reconcile::reference(name, &projected.key).into();
            for (n, (before, after)) in projected.peers {
                edit.peer_change(name, &n, &before, &after);
            }
        }
        self.verify_local_providers(old, next, &edit)?;
        let mut body = edit.body;
        let result = self.parent(old, next, &mut body, edit.peers)?;
        if let Some(existing) = self.records.get(&result.key) {
            if existing != &body {
                return Err(missing(
                    &["snapshots", &result.key],
                    "two paths require different contents for the same instance",
                ));
            }
        } else {
            self.records.insert(result.key.clone(), body);
        }
        self.active.remove(&pair);
        self.memo.insert(pair, result.clone());
        Ok(result)
    }

    fn apply_rules(
        &mut self,
        old: &str,
        next: &str,
        edit: &mut SnapshotEdit,
    ) -> Result<(), LockfileError> {
        if self.policy.unchanged(old) {
            return Ok(());
        }
        let [base, donor] = self.docs;
        let pkg = graph::package_key(old);
        let previous = &base["snapshots"][old];
        let installed = &donor["snapshots"][next];
        let rules = self
            .policy
            .effective_extensions(old)
            .ok_or_else(|| missing(&["packageExtensions", pkg], "invalid extension declaration"))?;
        if rules[0][2..] != rules[1][2..] {
            return Err(missing(
                &["packageExtensions", pkg, "peerDependencies"],
                "changing declared peer requirements needs a new resolution",
            ));
        }
        let changed: BTreeSet<_> = rules[0][..2]
            .iter()
            .zip(&rules[1][..2])
            .flat_map(|(before, after)| {
                before
                    .keys()
                    .chain(after.keys())
                    .copied()
                    .filter(move |n| before.get(n) != after.get(n))
            })
            .collect();
        let (parent, version) = pkg.rsplit_once('@').expect("matched package selector");
        let parent = serde_json::json!({"name":parent,"version":version});
        for name in changed {
            let declaration = rules[1][1]
                .get(name)
                .or_else(|| rules[1][0].get(name))
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        missing(
                            &["packageExtensions", pkg, name],
                            "expected a dependency specifier",
                        )
                    })
                })
                .transpose()?;
            let constraint = declaration
                .map(|spec| self.overrides.apply(&parent, ".", name, spec))
                .transpose()?
                .flatten();
            let old_edge = dependency(previous, name);
            let installed_edge = dependency(installed, name);
            if let Some(spec) = constraint.as_deref() {
                if installed_edge.is_none()
                    && rules[1][0].contains_key(name)
                    && !rules[1][1].contains_key(name)
                {
                    return Err(missing(
                        &["snapshots", next, name],
                        "installed snapshot is missing the required extension dependency",
                    ));
                }
                if old_edge.is_none()
                    && let Some((_, pin)) = installed_edge
                    && !reconcile::accepts(name, spec, pin, donor)
                {
                    return Err(missing(
                        &["snapshots", next, name],
                        "installed pin does not satisfy the extension",
                    ));
                }
            }
            // A range edit permits the original version even if pnpm updated
            // the same dependency elsewhere. Keep its complete peer instance.
            let new_edge = match (old_edge, installed_edge, constraint.as_deref()) {
                (Some((_, old_pin)), Some((group, _)), Some(spec))
                    if reconcile::accepts(name, spec, old_pin, base) =>
                {
                    Some((group, old_pin))
                }
                _ => installed_edge,
            };
            if old_edge == new_edge {
                continue;
            }
            let body = &mut edit.body;
            if let Some((g, _)) = old_edge {
                let map = body[g].as_object_mut().expect("existing dependency group");
                map.remove(name);
                if map.is_empty() {
                    body.as_object_mut().unwrap().remove(g);
                }
            }
            if let Some((g, pin)) = new_edge {
                body[g][name] = pin.into();
            }
            if old_edge.map(|e| e.1) != new_edge.map(|e| e.1) {
                edit.touched.insert(name.to_owned());
                let mut interfaces = [Interface::new(), Interface::new()];
                for (side, edge) in [old_edge, new_edge].into_iter().enumerate() {
                    if let Some((_, pin)) = edge {
                        let key = graph::snapshot_key(
                            name,
                            pin,
                            graph::section(self.docs[side], "snapshots"),
                        );
                        if self.docs[side]["snapshots"].get(&key).is_none() {
                            return Err(missing(
                                &["snapshots", &key],
                                "missing locked extension dependency",
                            ));
                        }
                        interfaces[side] = self.interface(side, &key)?;
                        if side == 1 {
                            self.new_children.insert(key);
                        }
                    }
                }
                for n in interfaces[0].keys().chain(interfaces[1].keys()) {
                    edit.peer_change(
                        name,
                        n,
                        interfaces[0].get(n).unwrap_or(&Binding::Absent),
                        interfaces[1].get(n).unwrap_or(&Binding::Absent),
                    );
                }
            }
        }
        Ok(())
    }

    fn materialize(mut self, output: &mut Value) -> Result<(), LockfileError> {
        let donor = self.docs[1];
        while let Some(key) = self.new_children.pop_first() {
            if self.records.contains_key(&key) {
                continue;
            }
            if output["snapshots"].get(&key).is_some() {
                if self.affected.contains(&key) {
                    self.visit(&key, &key)?;
                }
                continue;
            }
            let body = donor["snapshots"]
                .get(&key)
                .ok_or_else(|| missing(&["snapshots", &key], "missing installed dependency"))?;
            let release = graph::package_key(&key);
            let package = donor["packages"]
                .get(release)
                .ok_or_else(|| missing(&["packages", release], "missing installed package"))?;
            if let Some(old) = output["packages"].get(release) {
                if old.get("resolution") != package.get("resolution") {
                    return Err(missing(
                        &["packages", release],
                        "installed package artifact differs from baseline",
                    ));
                }
            } else {
                output["packages"][release] = package.clone();
            }
            for (_, n, v) in edges(body) {
                let r = v.as_str().ok_or_else(|| {
                    missing(&["snapshots", &key, n], "invalid dependency reference")
                })?;
                if !r.starts_with("link:") {
                    self.new_children.insert(graph::snapshot_key(
                        n,
                        r,
                        graph::section(donor, "snapshots"),
                    ));
                }
            }
            output["snapshots"][key] = body.clone();
        }
        for (k, v) in self.records {
            output["snapshots"][k] = v;
        }
        Ok(())
    }
}
