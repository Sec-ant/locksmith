//! Decide which parts of the previous lockfile survive a generation: whole
//! graph reuse while the lockfile is current, the releases a changed input
//! invalidates, and the historical instances whose evaluated manifests still
//! reproduce their recorded snapshots.
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use super::{Baseline, Build, Draft, Node, RegistryPackage, Result, SourceManifest, Sources};
use crate::{
    Value, WorkspaceConfig,
    generation::{incremental, project_times},
    graph, manifest, reconcile,
};

impl Node {
    /// A second occurrence of the same evaluated publication can witness its
    /// ordinary releases. The historical snapshot supplies its own peer scope.
    pub(in crate::generation) fn matches_history(
        &self,
        old: &Value,
        key: &str,
        body: &Value,
        config: &WorkspaceConfig,
    ) -> bool {
        let previous = &old["packages"][&self.package];
        if ["peerDependencies", "peerDependenciesMeta"]
            .iter()
            .any(|field| previous.get(field) != self.record.get(field))
            || crate::peer::patch(key) != self.patch.as_deref()
        {
            return false;
        }
        let mut external = BTreeSet::new();
        if let Some(peers) = graph::section(&self.record, "peerDependencies") {
            let (bound, opaque) = external_peers(old, key, body, peers, config);
            for name in peers.keys() {
                let Some((group, _)) = graph::dependency(body, name) else {
                    if self.edges.contains_key(name) {
                        return false;
                    }
                    continue;
                };
                if bound.contains(name.as_str()) {
                    let group_wanted =
                        if self.record["peerDependenciesMeta"][name]["optional"] == true {
                            "optionalDependencies"
                        } else {
                            "dependencies"
                        };
                    if group != group_wanted {
                        return false;
                    }
                    external.insert(name.as_str());
                } else if !self.edges.contains_key(name)
                    || opaque
                    || !reconcile::published::records_fallback_peers(config)
                {
                    // A bare edge with a newly declared peer could be an old
                    // ordinary dependency. Rebind it in the actual caller.
                    return false;
                }
            }
        }
        let snapshots = graph::section(old, "snapshots");
        for (name, dep) in &self.edges {
            if external.contains(name.as_str()) {
                continue;
            }
            let Some(pin) = body[dep.group][name].as_str() else {
                return false;
            };
            if pin.starts_with("link:") {
                if pin != dep.target {
                    return false;
                }
            } else if graph::snapshot_key(name, pin, snapshots) != dep.target {
                return false;
            }
        }
        graph::snapshot_dependencies(body).all(|(group, name, _)| {
            external.contains(name.as_str())
                || self.edges.get(name).is_some_and(|d| d.group == group)
        })
    }
}

fn external_peers<'p>(
    old: &Value,
    key: &str,
    body: &Value,
    peers: &'p serde_json::Map<String, Value>,
    config: &WorkspaceConfig,
) -> (BTreeSet<&'p str>, bool) {
    if let Some(tokens) = crate::peer::explicit_bindings(key) {
        let external = peers.keys().filter_map(|name| {
            let (_, pin) = graph::dependency(body, name)?;
            let (actual, _) = crate::peer::canonical_binding(name, pin);
            tokens
                .get(actual)
                .is_some_and(|token| crate::peer::token_matches(name, pin, actual, token))
                .then_some(name.as_str())
        });
        return (external.collect(), false);
    }

    let mut direct: BTreeMap<_, _> = peers
        .keys()
        .filter_map(|name| Some((name.clone(), graph::dependency(body, name)?.1.to_owned())))
        .collect();
    if let Some(hash) = crate::peer::patch(key) {
        direct.insert("patch_hash".into(), hash.into());
    }
    // Opaque suffixes prove a binding when all recorded pins reproduce the hash.
    for name in graph::transitive(body) {
        if let Some(pin) = transitive_pin(old, body, name, 8) {
            direct.insert(name.to_owned(), pin);
        }
    }
    let matches = crate::peer::matches_bindings(
        key,
        &direct,
        config.resolution.lock.dedupe_peers,
        config.linked_peer_encoding,
    );
    let external = peers
        .keys()
        .filter(|name| matches && direct.contains_key(*name));
    (external.map(String::as_str).collect(), true)
}

/// The provider of `name` that a historical instance received for one of its
/// descendants: the binding of the child that declares the peer, or the token
/// a child printed in its own identity. Hashed identities hide their tokens,
/// so search such a child's children, up to `depth` levels.
fn transitive_pin(old: &Value, body: &Value, name: &str, depth: usize) -> Option<String> {
    let snapshots = graph::section(old, "snapshots");
    for (_, child, pin) in graph::snapshot_dependencies(body) {
        let Some(pin) = pin.as_str().filter(|p| !p.starts_with("link:")) else {
            continue;
        };
        let key = graph::snapshot_key(child, pin, snapshots);
        let inner = &old["snapshots"][&key];
        if old["packages"][graph::package_key(&key)]["peerDependencies"]
            .get(name)
            .is_some()
        {
            if let Some((_, bound)) = graph::dependency(inner, name) {
                return Some(bound.to_owned());
            }
            continue;
        }
        if !graph::transitive(inner).any(|peer| peer == name) {
            continue;
        }
        match crate::peer::explicit_bindings(&key) {
            Some(tokens) => {
                if let Some(token) = tokens.get(name) {
                    return Some((*token).to_owned());
                }
            }
            None if depth > 0 => {
                if let Some(found) = transitive_pin(old, inner, name, depth - 1) {
                    return Some(found);
                }
            }
            None => {}
        }
    }
    None
}

impl<'a> Baseline<'a> {
    fn invalid_releases(
        &self,
        cache: &mut Sources<'_>,
        live: &BTreeSet<&'a str>,
    ) -> Result<BTreeSet<&'a str>> {
        let mut refresh = BTreeSet::new();
        let patches_changed =
            self.old.get("patchedDependencies") != self.header.get("patchedDependencies");
        let pins: BTreeMap<_, _> = live
            .iter()
            .filter_map(|&key| RegistryPackage::from_key(key).map(|entry| (key, entry)))
            .collect();
        cache
            .source
            .prepare_pins(&pins.values().map(|(package, _)| package.clone()).collect())?;
        for &key in live {
            let record = &self.old["packages"][key];
            let locator = manifest::selector(key).1.unwrap_or_default();
            if locator.starts_with("file:") || record["resolution"]["type"] == "directory" {
                refresh.insert(key);
            } else if let Some((package, version)) = pins.get(key)
                && !cache.accepts_pin(package, version, false)?
            {
                refresh.insert(key);
            } else if patches_changed {
                let before = reconcile::desired_patch(
                    key,
                    self.old,
                    [self.old, &Value::Null, &Value::Null, &Value::Null],
                );
                let after = reconcile::desired_patch(
                    key,
                    self.header,
                    [self.old, &Value::Null, &Value::Null, &Value::Null],
                );
                if before != after {
                    refresh.insert(key);
                }
            }
        }
        if let Some(changed) = self.changed_overrides.as_ref().filter(|s| !s.is_empty()) {
            for (key, body) in graph::section(self.old, "snapshots").into_iter().flatten() {
                let base = graph::package_key(key);
                if live.contains(base)
                    && incremental::observes_override(&self.old["packages"][base], body, changed)
                {
                    refresh.insert(base);
                }
            }
        }
        Ok(refresh)
    }
}

/// Whether a release's recorded peers and snapshot children still satisfy the
/// declarations of its evaluated manifest.
fn declarations_match(
    snapshot: &str,
    body: &Value,
    record: &Value,
    release: &Rc<SourceManifest>,
    build: &mut Build<'_, '_, '_>,
) -> Result<bool> {
    let base = build.base;
    let config = build.config;
    let input = base.publication(".", release)?;
    let manifest = &release.manifest;
    let declared = &input.ordinary;
    let recorded_optional = |name: &str| record["peerDependenciesMeta"][name]["optional"] == true;
    let recorded_peers: BTreeMap<_, _> = graph::section(record, "peerDependenciesMeta")
        .into_iter()
        .flat_map(|entries| entries.keys())
        .filter(|name| recorded_optional(name))
        .map(|name| (name.as_str(), (Some("*"), true)))
        .chain(
            graph::section(record, "peerDependencies")
                .into_iter()
                .flatten()
                .map(|(name, range)| (name.as_str(), (range.as_str(), recorded_optional(name)))),
        )
        .collect();
    // Older locks omit a peer declaration when the package supplies it itself.
    let fallback = reconcile::published::records_fallback_peers(config);
    let evaluated = input.peers.iter().filter(|(name, _)| {
        fallback || !declared.contains_key(*name) || record["peerDependencies"].get(*name).is_some()
    });
    if !recorded_peers.into_iter().eq(evaluated
        .map(|(name, (range, optional))| (name.as_str(), (Some(range.as_str()), *optional))))
    {
        return Ok(false);
    }
    if graph::snapshot_dependencies(body).any(|(_, child, _)| {
        !declared.contains_key(child)
            && !input.peers.contains_key(child)
            && record["peerDependencies"].get(child).is_none()
    }) {
        return Ok(false);
    }
    let external = graph::section(record, "peerDependencies")
        .filter(|peers| declared.keys().any(|name| peers.contains_key(name)))
        .map_or_else(BTreeSet::new, |peers| {
            external_peers(build.old, snapshot, body, peers, config).0
        });
    for (dependency, (group, spec)) in declared {
        if external.contains(dependency.as_str()) {
            continue;
        }
        let Some((_, pin)) = graph::dependency(body, dependency)
            .filter(|(recorded_group, _)| recorded_group == group)
        else {
            return Ok(false);
        };
        if !build.keeps_pin(manifest, dependency, spec, pin)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) struct Validation<'a> {
    epoch: u64,
    outdated: bool,
    roots: Vec<usize>,
    invalid: BTreeSet<&'a str>,
}

impl<'b> Build<'_, 'b, '_> {
    /// Evaluate each historical parent before following its recorded children.
    /// Expansion selects fresh children when the parent can no longer be kept.
    fn evaluate_history(
        &mut self,
        draft: &mut Draft,
        outdated: bool,
        roots: &[usize],
    ) -> Result<BTreeSet<&'b str>> {
        let hooks = outdated && self.cache.reads_packages();
        let mut stale = BTreeSet::new();
        let mut live = BTreeSet::new();
        let history = self.base.partition.history();
        let mut pending = roots.to_vec();
        let mut seen = vec![false; history.snapshots.len()];
        while let Some(id) = pending.pop() {
            if std::mem::replace(&mut seen[id], true) {
                continue;
            }
            let (key, node) = history.snapshots.get_index(id).expect("scheduled snapshot");
            let base = node.package;
            let Some(record) = node.record else {
                continue;
            };
            if stale.contains(base) {
                continue;
            }
            let first = live.insert(base);
            let locator = manifest::selector(base).1.unwrap_or_default();
            let release =
                if locator.starts_with("file:") || record["resolution"]["type"] == "directory" {
                    // The local manifest is re-read by ordinary expansion.
                    continue;
                } else if outdated && self.base.compat_candidates.contains(base)
                    || hooks && RegistryPackage::from_key(base).is_some()
                {
                    Some(self.cache.release(base)?)
                } else if hooks
                    && (record["resolution"]["type"] == "git"
                        || locator.starts_with("http://")
                        || locator.starts_with("https://"))
                {
                    Some(self.cache.locked_source(base, &record["resolution"])?)
                } else {
                    None
                };
            if let Some(release) = release {
                let release = Rc::clone(release);
                if hooks && first {
                    reconcile::published::refresh_installation_metadata(
                        &mut draft.document["packages"][base],
                        &release.manifest,
                    );
                }
                if !declarations_match(key, node.body, record, &release, self)? {
                    stale.insert(base);
                    continue;
                }
            }
            pending.extend(&node.children);
        }
        stale.extend(self.base.invalid_releases(self.cache, &live)?);
        Ok(stale)
    }

    /// pnpm resolves the whole graph again, reading every manifest, once an
    /// importer, catalog, override or patch differs from the lockfile.
    fn lockfile_outdated(&self, draft: &Draft) -> bool {
        let importers = graph::section(self.old, "importers");
        fn specifiers(doc: &Value) -> BTreeMap<(&str, &str), &Value> {
            graph::section(doc, "catalogs")
                .into_iter()
                .flatten()
                .flat_map(|(catalog, entries)| {
                    entries
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(move |(name, entry)| {
                            ((catalog.as_str(), name.as_str()), &entry["specifier"])
                        })
                })
                .collect()
        }
        self.base
            .changed_overrides
            .as_ref()
            .is_none_or(|changed| !changed.is_empty())
            || self.old.get("patchedDependencies") != self.header.get("patchedDependencies")
            || draft
                .roots
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
                != importers
                    .into_iter()
                    .flat_map(|m| m.keys())
                    .map(String::as_str)
                    .collect()
            || draft.roots.keys().any(|id| {
                let current = &draft.document["importers"][id];
                let previous = &self.old["importers"][id];
                manifest::GROUPS.into_iter().any(|group| {
                    let before = graph::section(previous, group);
                    let after = graph::section(current, group);
                    before.map_or(0, |m| m.len()) != after.map_or(0, |m| m.len())
                        || after.into_iter().flatten().any(|(alias, dep)| {
                            dep["specifier"] != previous[group][alias]["specifier"]
                        })
                })
            })
            || specifiers(&draft.document) != specifiers(self.old)
    }

    /// Keep the existing physical graph when importer edits leave its peer scopes intact.
    /// In particular, dropping an ordinary workspace link must not re-resolve a
    /// historical missing peer somewhere else in the workspace.
    pub(in crate::generation) fn retain_graph(
        &mut self,
        draft: &mut Draft,
    ) -> Result<Option<Value>> {
        // pnpm runs readPackage on every manifest whenever it resolves, and a
        // hook can read inputs outside the file its checksum covers. Once the
        // lockfile is outdated, every retained release runs the hook again below.
        let hooks = self.cache.reads_packages();
        let declarations_changed =
            self.base.declaration_inputs_changed || self.base.changed_overrides.is_none();
        if self.old.get("settings") != self.header.get("settings") || !hooks && declarations_changed
        {
            return Ok(None);
        }
        let outdated = declarations_changed
            || hooks && self.old["pnpmfileChecksum"].is_null()
            || self.lockfile_outdated(draft);
        self.seeded_history = true;
        self.rereads_manifests = outdated;
        for field in ["packages", "snapshots"] {
            if let Some(v) = self.old.get(field) {
                draft.document[field] = v.clone();
            }
        }
        let history = self.base.partition.history();
        let roots: Vec<_> = draft
            .roots
            .values()
            .flat_map(|deps| deps.values())
            .filter_map(|dep| history.snapshots.get_index_of(dep.target.as_str()))
            .collect();
        let mut validation = self.base.history_validation.borrow_mut();
        let epoch = self.cache.policy_epoch;
        // A hook pass also refreshes the output metadata for this draft.
        if outdated && hooks
            || validation.as_ref().is_none_or(|prior| {
                prior.epoch != epoch || prior.outdated != outdated || prior.roots != roots
            })
        {
            let invalid = self.evaluate_history(draft, outdated, &roots)?;
            *validation = Some(Validation {
                epoch,
                outdated,
                roots,
                invalid,
            });
        }
        let invalid = &validation.as_ref().expect("validated history").invalid;
        let incomplete = self.base.partition.incomplete(|key, _| {
            invalid.contains(graph::package_key(key))
                || self.reopened.contains(key)
                || self.pinned_snapshots.contains(key)
        });
        let prune = self.base.partition.apply(draft, &incomplete) || !incomplete.is_empty();
        self.pending = draft
            .roots
            .values()
            .flat_map(|deps| deps.values())
            .filter(|d| !d.locked && !d.target.starts_with("link:"))
            .map(|d| d.target.clone())
            .collect();
        let doc = &mut draft.document;
        if let Some(records) = doc.get_mut("snapshots").and_then(Value::as_object_mut) {
            for key in incomplete {
                records.remove(&key);
            }
        }
        if !self.pending.is_empty() {
            return Ok(None);
        }
        if prune {
            graph::prune(doc, crate::DocumentKind::Project)?;
            graph::refresh_optional(doc)?;
        }
        project_times(doc, self.old, self.config);
        Ok(Some(std::mem::take(doc)))
    }
}
