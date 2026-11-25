//! Project a workspace rule change onto the baseline, using installed instances
//! as witnesses. Only paths to releases matched by a changed rule are paired.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::{
    Lockfile, LockfileError, Value, WorkspaceConfig, config, graph, manifest, peer, reconcile,
};
use graph::{dependency, transitive};
use reconcile::missing;

const EDGES: [&str; 2] = ["dependencies", "optionalDependencies"];
mod bindings;
mod rules;
use rules::RootChoices;
pub(super) use rules::{RootChoice, checked_overrides};
pub(super) use rules::{check_installed_rules, override_names};

/// Combining inputs may require incompatible identities even when each input is valid.
pub(super) enum ProjectionError {
    MixedPeers,
    Input(LockfileError),
}

impl From<LockfileError> for ProjectionError {
    fn from(value: LockfileError) -> Self {
        Self::Input(value)
    }
}

pub(super) struct Projection {
    pub(super) roots: RootChoices,
    bindings: Vec<bindings::Root>,
    pub(super) unprojected: BTreeSet<String>,
}
use bindings::{Binding, Contributions, Edits, Interface};

#[derive(Clone)]
struct Instance {
    key: String,
    peers: Edits,
}

pub(super) fn edges(body: &Value) -> impl Iterator<Item = (&'static str, &String, &Value)> {
    EDGES.into_iter().flat_map(|g| {
        graph::section(body, g)
            .into_iter()
            .flatten()
            .map(move |(n, v)| (g, n, v))
    })
}

pub(super) fn checked_checksum(doc: &Value, cfg: &WorkspaceConfig) -> Result<(), LockfileError> {
    if crate::checksum::extensions_match(cfg, doc.get("packageExtensionsChecksum")) {
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
    registry: Option<&crate::RegistryMetadata>,
) -> Result<Option<(Lockfile, Projection)>, ProjectionError> {
    fn rules(c: &WorkspaceConfig) -> Option<&serde_json::Map<String, Value>> {
        c.resolution
            .package_extensions
            .as_ref()
            .and_then(Value::as_object)
    }
    let base = baseline
        .project()
        .ok_or_else(|| missing(&[], "missing baseline project lockfile"))?;
    let donor = candidate
        .project()
        .ok_or_else(|| missing(&[], "missing installed project lockfile"))?;
    let root = manifests.iter().find(|m| m.importer == ".");
    let root_reference_changed = after
        .overrides
        .iter()
        .flatten()
        .filter_map(|(_, v)| v.strip_prefix('$'))
        .any(|name| {
            root.is_some_and(|root| {
                manifest::GROUPS.iter().any(|g| {
                    root.before.and_then(|r| r.get(g)?.get(name))
                        != root.after.and_then(|r| r.get(g)?.get(name))
                })
            })
        });
    let changes_overrides = before.overrides != after.overrides
        || root_reference_changed
        || !manifest::same_override_order(base, donor);
    let changes_extensions = !rules(before)
        .into_iter()
        .flatten()
        .eq(rules(after).into_iter().flatten());
    if before == after && !changes_overrides && !changes_extensions {
        return Ok(None);
    }
    let mut expected = before.clone();
    expected
        .resolution
        .package_extensions
        .clone_from(&after.resolution.package_extensions);
    expected.catalogs.clone_from(&after.catalogs);
    expected.overrides.clone_from(&after.overrides);
    if expected != *after || base["settings"] != donor["settings"] {
        return Err(missing(
            &["configuration"],
            "other workspace resolution settings changed",
        )
        .into());
    }
    checked_checksum(base, before)?;
    checked_checksum(donor, after)?;
    let docs = [base, donor];
    let configs = [before, after];
    let header = if changes_overrides
        || before.catalogs != after.catalogs
            && after
                .overrides
                .iter()
                .flatten()
                .any(|(_, v)| v.starts_with("catalog:"))
    {
        rules::checked_overrides(base, before, root.and_then(|m| m.before))?;
        Some(rules::checked_overrides(
            donor,
            after,
            root.and_then(|m| m.after),
        )?)
    } else {
        None
    };
    let names = if changes_overrides {
        rules::override_names(docs)
    } else {
        BTreeSet::new()
    };
    let overrides = manifest::Overrides::new(donor, after);
    let mut override_edits = BTreeMap::<&str, BTreeSet<&str>>::new();
    if !names.is_empty() {
        let previous = manifest::Overrides::new(base, before);
        for doc in docs {
            for (key, body) in graph::section(doc, "snapshots").into_iter().flatten() {
                let pkg = graph::package_key(key);
                if !pkg.contains('@') {
                    continue;
                }
                for name in edges(body).map(|(_, n, _)| n.as_str()).chain(
                    graph::section(&doc["packages"][pkg], "peerDependencies")
                        .into_iter()
                        .flatten()
                        .map(|(n, _)| n.as_str()),
                ) {
                    if names.contains(name) {
                        override_edits.entry(pkg).or_default().insert(name);
                    }
                }
            }
        }
        // One release can have many peer instances in both locks. Match each
        // parent/child selector once, independent of the number of instances.
        override_edits.retain(|pkg, children| {
            let (parent, version) = pkg.rsplit_once('@').expect("versioned package");
            children.retain(|name| {
                previous.relevant(name, parent, version)
                    != overrides.relevant(name, parent, version)
            });
            !children.is_empty()
        });
    }
    let policy = config::ResolutionCompatibility::new(&before.resolution, &after.resolution, base);
    let affected = if changes_extensions || !override_edits.is_empty() {
        super::affected_by(base, |key| {
            let pkg = graph::package_key(key);
            !policy.unchanged(key)
                || override_edits.get(pkg).is_some_and(|names| {
                    donor["snapshots"].get(key).is_none()
                        || names.iter().any(|n| {
                            dependency(&base["snapshots"][key], n)
                                != dependency(&donor["snapshots"][key], n)
                                || ["peerDependencies", "peerDependenciesMeta"]
                                    .iter()
                                    .any(|f| {
                                        base["packages"][pkg][*f].get(*n)
                                            != donor["packages"][pkg][*f].get(*n)
                                    })
                        })
                })
        })
    } else {
        BTreeSet::new()
    };
    let mut output = base.clone();
    let catalog_roots =
        if before.catalogs != after.catalogs || base.get("catalogs") != donor.get("catalogs") {
            rules::catalogs(&mut output, docs, configs, &affected)?
        } else {
            RootChoices::new()
        };
    if let Some(header) = header {
        if let Some(v) = header {
            output["overrides"] = v;
        } else {
            output.as_object_mut().unwrap().remove("overrides");
        }
    }
    let roots = rules::importer_choices(docs, configs, &output, &names, catalog_roots)?;
    if let Some(checksum) = donor.get("packageExtensionsChecksum") {
        output["packageExtensionsChecksum"] = checksum.clone();
    } else {
        output
            .as_object_mut()
            .expect("project mapping")
            .remove("packageExtensionsChecksum");
    }
    let mut bindings = Vec::new();
    let mut unprojected = BTreeSet::new();
    if !affected.is_empty() {
        let mut walk = Projector {
            docs: [base, donor],
            policy,
            config: after,
            overrides,
            override_edits,
            registry,
            packages: BTreeMap::new(),
            affected,
            peers: peer::Merger::new([Some(base), Some(donor), None]),
            active: HashSet::new(),
            cycles: HashMap::new(),
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
                        return Err(
                            missing(&["importers", id, name], "missing baseline pin").into()
                        );
                    };
                    let old = graph::snapshot_key(name, pin, graph::section(base, "snapshots"));
                    if roots
                        .get(id)
                        .and_then(|r| r.get(name))
                        .is_some_and(|r| *r != RootChoice::Baseline)
                        || !walk.affected.contains(&old)
                    {
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
                        )
                        .into());
                    }
                    if current.get("specifier") != dep.get("specifier")
                        && manifests
                            .iter()
                            .any(|m| m.importer == id && m.before != m.after)
                    {
                        let fits = current["specifier"]
                            .as_str()
                            .is_some_and(|s| reconcile::accepts(name, s, pin, &output));
                        if !fits {
                            continue;
                        }
                    }
                    let next = walk.witness(name, &old, current.get("version"))?;
                    let selected = walk.visit(&old, &next)?;
                    output["importers"][id][group][name]["version"] =
                        reconcile::reference(name, &selected.key).into();
                    if !selected.peers.is_empty() {
                        bindings.push(bindings::Root {
                            id: id.clone(),
                            group,
                            name: name.clone(),
                            instance: selected,
                        });
                    }
                }
            }
        }
        unprojected = walk.materialize(&mut output)?;
    }
    Ok(Some((
        baseline.converged_project(output, candidate),
        Projection {
            roots,
            bindings,
            unprojected,
        },
    )))
}

struct SnapshotEdit {
    body: Value,
    touched: BTreeSet<String>,
    peers: Contributions,
    declared: Edits,
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
    override_edits: BTreeMap<&'a str, BTreeSet<&'a str>>,
    registry: Option<&'a crate::RegistryMetadata>,
    packages: BTreeMap<String, Value>,
    affected: BTreeSet<String>,
    peers: peer::Merger<'a>,
    active: HashSet<(String, String)>,
    cycles: HashMap<(String, String), Instance>,
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

    fn visit(&mut self, old: &str, next: &str) -> Result<Instance, ProjectionError> {
        let pair = (old.to_owned(), next.to_owned());
        if let Some(result) = self.memo.get(&pair) {
            return Ok(result.clone());
        }
        if !self.active.insert(pair.clone()) {
            // An edge back to an active frame can use the installed identity
            // provisionally. The owner must later reproduce its entire peer delta.
            let peers = if old == next
                && transitive(&self.docs[0]["snapshots"][old])
                    .eq(transitive(&self.docs[1]["snapshots"][next]))
                && self.docs[0]["packages"][graph::package_key(old)].get("peerDependencies")
                    == self.docs[1]["packages"][graph::package_key(next)].get("peerDependencies")
            {
                Edits::new()
            } else {
                let before = self.interface(0, old)?;
                let after = self.interface(1, next)?;
                before
                    .keys()
                    .chain(after.keys())
                    .filter_map(|n| {
                        let change = (
                            before.get(n).cloned().unwrap_or_default(),
                            after.get(n).cloned().unwrap_or_default(),
                        );
                        (change.0 != change.1).then(|| (n.clone(), change))
                    })
                    .collect()
            };
            let assumed = Instance {
                key: next.into(),
                peers,
            };
            self.cycles.insert(pair, assumed.clone());
            return Ok(assumed);
        }
        let [base, donor] = self.docs;
        let pkg = graph::package_key(old);
        if base["packages"][pkg].get("resolution") != donor["packages"][pkg].get("resolution")
            || !base["packages"][pkg]["resolution"]["integrity"].is_string()
        {
            return Err(missing(
                &["packages", pkg],
                "workspace edit requires the same verified package artifact",
            )
            .into());
        }
        let previous = &base["snapshots"][old];
        let installed = &donor["snapshots"][next];
        let mut edit = SnapshotEdit {
            body: previous.clone(),
            touched: BTreeSet::new(),
            peers: Contributions::new(),
            declared: Edits::new(),
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
        self.verify_local_providers(old, next, &mut edit)?;
        let mut body = edit.body;
        let result = self.parent(old, next, &mut body, edit.peers, edit.declared)?;
        if let Some(existing) = self.records.get(&result.key) {
            if existing != &body {
                return Err(ProjectionError::MixedPeers);
            }
        } else {
            self.records.insert(result.key.clone(), body);
        }
        if self
            .cycles
            .remove(&pair)
            .is_some_and(|assumed| assumed.key != result.key || assumed.peers != result.peers)
        {
            return Err(ProjectionError::MixedPeers);
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
        let [base, donor] = self.docs;
        let pkg = graph::package_key(old);
        let ext = !self.policy.unchanged(old);
        let overridden = self.override_edits.get(pkg).cloned().unwrap_or_default();
        let rules = if ext {
            self.policy.effective_extensions(old).ok_or_else(|| {
                missing(&["packageExtensions", pkg], "invalid extension declaration")
            })?
        } else {
            Default::default()
        };
        let old_pkg = &base["packages"][pkg];
        let new_pkg = &donor["packages"][pkg];
        let mut changed = overridden.clone();
        for (before, after) in rules[0][..2].iter().zip(&rules[1][..2]) {
            changed.extend(
                before
                    .keys()
                    .chain(after.keys())
                    .copied()
                    .filter(|n| before.get(n) != after.get(n)),
            );
        }
        let mut peer_names = BTreeSet::new();
        for (index, field) in ["peerDependencies", "peerDependenciesMeta"]
            .into_iter()
            .enumerate()
        {
            let before = &rules[0][index + 2];
            let after = &rules[1][index + 2];
            let allowed = |n: &str| before.get(n) != after.get(n) || overridden.contains(n);
            for n in [old_pkg, new_pkg]
                .into_iter()
                .filter_map(|p| graph::section(p, field))
                .flat_map(|m| m.keys())
            {
                if old_pkg[field].get(n) != new_pkg[field].get(n) {
                    if !allowed(n) {
                        return Err(missing(
                            &["packages", pkg, field, n],
                            "peer declaration changed outside the edited workspace rules",
                        ));
                    }
                    peer_names.insert(n.as_str());
                }
            }
            for (&name, &declaration) in after {
                if before.get(name) != Some(&declaration)
                    && old_pkg[field].get(name).is_none()
                    && !overridden.contains(name)
                    && new_pkg[field].get(name) != Some(declaration)
                {
                    return Err(missing(
                        &["packages", pkg, field, name],
                        "installed package is missing the declared peer extension",
                    ));
                }
            }
        }
        // A changed ordinary dependency or override can supply an existing peer
        // without changing the peer range published in the package record.
        for &name in &changed {
            if (old_pkg["peerDependencies"].get(name).is_some()
                || new_pkg["peerDependencies"].get(name).is_some())
                && dependency(&base["snapshots"][old], name)
                    != dependency(&donor["snapshots"][next], name)
            {
                peer_names.insert(name);
            }
        }
        if !peer_names.is_empty() {
            let package = self
                .packages
                .entry(pkg.into())
                .or_insert_with(|| old_pkg.clone());
            for field in ["peerDependencies", "peerDependenciesMeta"] {
                if let Some(v) = new_pkg.get(field) {
                    package[field] = v.clone();
                } else {
                    package.as_object_mut().unwrap().remove(field);
                }
            }
            let mut original = self.interface(0, old)?;
            let mut installed = self.interface(1, next)?;
            for &name in &peer_names {
                let before = original.remove(name).unwrap_or_default();
                let after = installed.remove(name).unwrap_or_default();
                if before != after {
                    edit.declared.insert(name.to_owned(), (before, after));
                }
            }
            changed.extend(peer_names.iter().copied());
        }
        if changed.is_empty() {
            return Ok(());
        }
        let previous = &base["snapshots"][old];
        let installed = &donor["snapshots"][next];
        let (parent, version) = pkg
            .rsplit_once('@')
            .ok_or_else(|| missing(&["packages", pkg], "expected a package identity"))?;
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
            let old_edge = dependency(previous, name);
            let installed_edge = dependency(installed, name);
            let relevant = self
                .overrides
                .relevant(name, parent["name"].as_str().unwrap(), version);
            let unconditional = overridden.contains(name)
                && !relevant.is_empty()
                && relevant
                    .iter()
                    .all(|(s, _)| manifest::override_selector(s).2.is_none());
            let published = if declaration.is_none()
                && overridden.contains(name)
                && !unconditional
                && old_edge != installed_edge
                && old_edge.is_some()
                && self
                    .registry
                    .is_some_and(|r| !r.unavailable_manifests.contains(pkg))
                && self.config.resolution.pnpmfile_checksum.is_none()
                && peer::patch(old).is_none()
            {
                let group = installed_edge.or(old_edge).unwrap().0;
                reconcile::published::declaration(
                    self.registry,
                    self.config,
                    new_pkg,
                    pkg,
                    group,
                    name,
                    "original dependency declaration is needed to retain the baseline pin",
                )?
            } else {
                None
            };
            let known = declaration
                .or(published.as_deref())
                .or(unconditional.then_some("*"));
            let constraint = known
                .map(|spec| self.overrides.apply(&parent, ".", name, spec))
                .transpose()?
                .flatten();
            if known.is_some() && constraint.is_none() && installed_edge.is_some() {
                return Err(missing(
                    &["snapshots", next, name],
                    "installed snapshot contains a dependency removed by an override",
                ));
            }
            let mut installed_edge = installed_edge;
            let recovered;
            if let Some(spec) = constraint.as_deref() {
                let required_missing = installed_edge.is_none()
                    && rules[1][0].contains_key(name)
                    && !rules[1][1].contains_key(name);
                let wrong_pin = (old_edge.is_none() || unconditional)
                    && !peer_names.contains(name)
                    && installed_edge.is_some_and(|(_, pin)| {
                        (spec.parse::<node_semver::Range>().is_ok()
                            || manifest::alias(spec).is_some())
                            && !reconcile::accepts(name, spec, pin, donor)
                    });
                if required_missing || wrong_pin {
                    if old_edge.is_none()
                        && self.config.resolution.pnpmfile_checksum.is_none()
                        && peer::patch(old).is_none()
                        && declaration.is_some_and(|d| d != spec)
                        && rules[1][0].contains_key(name)
                        && !rules[1][1].contains_key(name)
                        && new_pkg["peerDependencies"].get(name).is_none()
                        && let Some(pin) =
                            self.peers
                                .exact_override([old, next], self.config, name, spec)
                    {
                        recovered = pin;
                        installed_edge = Some(("dependencies", recovered.as_str()));
                    } else {
                        let reason = match installed_edge {
                            Some((_, pin)) => format!(
                                "installed pin {pin} does not satisfy the effective workspace rule {spec}"
                            ),
                            None => format!(
                                "installed snapshot is missing the required extension dependency {spec}"
                            ),
                        };
                        return Err(missing(&["snapshots", next, name], reason));
                    }
                }
            }
            // A range edit permits the original version even if pnpm updated
            // the same dependency elsewhere. Keep its complete peer instance.
            let new_edge = match (old_edge, installed_edge, constraint.as_deref()) {
                (Some((_, old_pin)), Some((group, _)), Some(spec))
                    if !peer_names.contains(name)
                        && reconcile::accepts(name, spec, old_pin, base) =>
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

    fn materialize(mut self, output: &mut Value) -> Result<BTreeSet<String>, ProjectionError> {
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
                    )
                    .into());
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
        for (pkg, body) in self.packages {
            output["packages"][pkg] = body;
        }
        self.affected.retain(|key| !self.records.contains_key(key));
        for (k, v) in self.records {
            output["snapshots"][k] = v;
        }
        bindings::insert_rebound(output, self.peers.into_generated())?;
        Ok(self.affected)
    }
}
