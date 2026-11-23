//! Decide which parts of the previous lockfile survive a generation: whole
//! graph reuse while the lockfile is current, the releases a changed input
//! invalidates, and the historical instances whose evaluated manifests still
//! reproduce their recorded snapshots.
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use super::{Baseline, Build, Draft, Node, RegistryPackage, Result, Sources};
use crate::{
    Value, WorkspaceConfig,
    generation::{incremental, peers, project_times},
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
        exact_instances: bool,
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
            let tokens = crate::peer::explicit_bindings(key);
            let mut direct = BTreeMap::new();
            for name in peers.keys() {
                if let Some((_, pin)) = graph::dependency(body, name) {
                    direct.insert(name.clone(), pin.to_owned());
                }
            }
            if let Some(hash) = &self.patch {
                direct.insert("patch_hash".into(), hash.clone());
            }
            // pnpm hashes every peer resolved for an instance, including peers
            // its descendants require. Their pins are recorded in the keys of
            // the children that bound them. An unresolved peer stays absent.
            if tokens.is_none() {
                for name in body["transitivePeerDependencies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if let Some(pin) = transitive_pin(old, body, name, 8) {
                        direct.insert(name.to_owned(), pin);
                    }
                }
            }
            // Opaque suffixes remain usable when the recorded peer pins
            // reproduce the hash exactly with this workspace's pnpm encoding.
            let exact_hash = tokens.is_none()
                && crate::peer::matches_bindings(
                    key,
                    &direct,
                    config.resolution.lock.dedupe_peers,
                    config.linked_peer_encoding,
                );
            for name in peers.keys() {
                let Some((group, pin)) = graph::dependency(body, name) else {
                    if self.edges.contains_key(name) {
                        return false;
                    }
                    continue;
                };
                let (actual, _) = crate::peer::canonical_binding(name, pin);
                let bound = exact_hash
                    || tokens.as_ref().is_some_and(|t| {
                        t.get(actual).is_some_and(|token| {
                            crate::peer::token_matches(name, pin, actual, token)
                        })
                    });
                if bound {
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
                    || tokens.is_none()
                    || !peers::records_fallback_peers(config)
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
            } else {
                let prior = graph::snapshot_key(name, pin, snapshots);
                if if exact_instances {
                    prior != dep.target
                } else {
                    graph::package_key(&prior) != graph::package_key(&dep.target)
                        || crate::peer::patch(&prior) != crate::peer::patch(&dep.target)
                } {
                    return false;
                }
            }
        }
        ["dependencies", "optionalDependencies"]
            .into_iter()
            .all(|group| {
                graph::section(body, group)
                    .into_iter()
                    .flatten()
                    .all(|(name, _)| {
                        external.contains(name.as_str())
                            || self.edges.get(name).is_some_and(|d| d.group == group)
                    })
            })
    }
}

/// The provider of `name` that a historical instance received for one of its
/// descendants: the binding of the child that declares the peer, or the token
/// a child printed in its own identity. Hashed identities hide their tokens,
/// so search such a child's children, up to `depth` levels.
fn transitive_pin(old: &Value, body: &Value, name: &str, depth: usize) -> Option<String> {
    let snapshots = graph::section(old, "snapshots");
    for group in ["dependencies", "optionalDependencies"] {
        for (child, pin) in graph::section(body, group).into_iter().flatten() {
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
            if !inner["transitivePeerDependencies"]
                .as_array()
                .is_some_and(|names| names.iter().any(|n| n == name))
            {
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
    }
    None
}

impl Baseline<'_> {
    fn incomplete(&self, cache: &mut Sources<'_>) -> Result<BTreeSet<String>> {
        let mut refresh = BTreeSet::new();
        let patches_changed =
            self.old.get("patchedDependencies") != self.header.get("patchedDependencies");
        for (key, record) in graph::section(self.old, "packages").into_iter().flatten() {
            let locator = manifest::selector(key).1.unwrap_or_default();
            if locator.starts_with("file:") || record["resolution"]["type"] == "directory" {
                refresh.insert(key.as_str());
            } else if let Some((package, version)) = RegistryPackage::from_key(key)
                && !cache.accepts_pin(&package, version, false)?
            {
                refresh.insert(key.as_str());
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
                    refresh.insert(key.as_str());
                }
            }
        }
        if let Some(changed) = self.changed_overrides.as_ref().filter(|s| !s.is_empty()) {
            for (key, body) in graph::section(self.old, "snapshots").into_iter().flatten() {
                let base = graph::package_key(key);
                if incremental::observes_override(&self.old["packages"][base], body, changed) {
                    refresh.insert(base);
                }
            }
        }
        Ok(self
            .partition
            .incomplete(|key, _| refresh.contains(graph::package_key(key))))
    }
}

impl<'a> Build<'a, '_> {
    /// pnpm runs readPackage for every release once it resolves, and a hook can
    /// read inputs outside the file its checksum covers. A release the hook
    /// leaves untouched resolves as its publication did. One the hook rewrites
    /// is stale when its recorded children no longer satisfy the declarations.
    fn rewritten_by_hook(&mut self) -> Result<BTreeSet<&'a str>> {
        let old: &'a Value = self.old;
        let snapshots = graph::section(old, "snapshots");
        let overridden: BTreeSet<&str> = graph::section(self.header, "overrides")
            .into_iter()
            .flatten()
            .map(|(rule, _)| manifest::override_scope(rule).0)
            .collect();
        let mut stale = BTreeSet::new();
        for (key, record) in graph::section(old, "packages").into_iter().flatten() {
            if RegistryPackage::from_key(key).is_none() {
                continue;
            }
            let manifest = &Rc::clone(self.cache.release(key)?).manifest;
            if !self.cache.hook_changed.contains(key) {
                continue;
            }
            let peers = &manifest["peerDependencies"];
            let declared: BTreeMap<&str, Option<&str>> = ["dependencies", "optionalDependencies"]
                .into_iter()
                .flat_map(|group| graph::section(manifest, group).into_iter().flatten())
                .filter(|(name, _)| peers.get(name.as_str()).is_none())
                .map(|(name, spec)| (name.as_str(), spec.as_str()))
                .collect();
            let satisfied = |body: &Value| {
                let recorded_children = ["dependencies", "optionalDependencies"]
                    .into_iter()
                    .flat_map(|group| graph::section(body, group).into_iter().flatten())
                    .all(|(name, _)| {
                        declared.contains_key(name.as_str())
                            || peers.get(name).is_some()
                            || record["peerDependencies"].get(name).is_some()
                    });
                recorded_children
                    && declared.iter().all(|(name, spec)| {
                        let Some((_, pin)) = graph::dependency(body, name) else {
                            return false;
                        };
                        if overridden.contains(name) {
                            return true;
                        }
                        let child = graph::snapshot_key(name, pin, snapshots);
                        let Some((_, version)) = RegistryPackage::from_key(&child) else {
                            return false;
                        };
                        spec.and_then(|spec| RegistryPackage::selector(name, spec))
                            .is_some_and(|(_, range)| {
                                range == "*" || manifest::matches_version(&range, Some(version))
                            })
                    })
            };
            if !snapshots
                .into_iter()
                .flatten()
                .filter(|(snapshot, _)| graph::package_key(snapshot) == key)
                .all(|(_, body)| satisfied(body))
            {
                stale.insert(key.as_str());
            }
        }
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
        let outdated = self.lockfile_outdated(draft);
        if self.cache.reads_packages() && self.old["pnpmfileChecksum"].is_null()
            || [
                "settings",
                "packageExtensionsChecksum",
                "ignoredOptionalDependencies",
                "pnpmfileChecksum",
            ]
            .iter()
            .any(|k| self.old.get(*k) != self.header.get(*k))
            || self.base.changed_overrides.is_none()
        {
            return Ok(None);
        }
        self.seeded_history = true;
        self.rereads_manifests = outdated;
        let mut stale = if outdated && self.cache.reads_packages() {
            self.rewritten_by_hook()?
        } else {
            BTreeSet::new()
        };
        if outdated {
            stale.extend(self.base.compat_stale.iter().copied());
        }
        let mut previous = self.base.incomplete.borrow_mut();
        if previous
            .as_ref()
            .is_none_or(|(epoch, _)| *epoch != self.cache.policy_epoch)
        {
            *previous = Some((self.cache.policy_epoch, self.base.incomplete(self.cache)?));
        }
        let incomplete = &previous.as_ref().expect("current publication policy").1;
        let expanded;
        let incomplete =
            if self.reopened.is_empty() && self.pinned_snapshots.is_empty() && stale.is_empty() {
                incomplete
            } else {
                expanded = self.base.partition.incomplete(|key, _| {
                    incomplete.contains(key)
                        || self.reopened.contains(key)
                        || self.pinned_snapshots.contains(key)
                        || stale.contains(graph::package_key(key))
                });
                &expanded
            };
        let prune = self.base.partition.apply(draft, incomplete) || !incomplete.is_empty();
        self.pending = draft
            .roots
            .values()
            .flat_map(|deps| deps.values())
            .filter(|d| !d.locked && !d.target.starts_with("link:"))
            .map(|d| d.target.clone())
            .collect();
        let doc = &mut draft.document;
        for field in ["packages", "snapshots"] {
            if let Some(v) = self.old.get(field) {
                doc[field] = v.clone();
            }
        }
        if let Some(records) = doc.get_mut("snapshots").and_then(Value::as_object_mut) {
            for key in incomplete {
                records.remove(key);
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

    /// Ordinary expansion reaches only releases with an ordinary path from an
    /// importer. Auto-installed peers and their subtrees appear later, during
    /// peer binding. Evaluate their historical occurrences too, so retention can
    /// witness them instead of re-selecting every peer-only provider.
    pub fn expand_history(&mut self, draft: &mut Draft) -> Result<()> {
        if self.seeded_history
            || !self.cache.reads_packages()
            || self.old.get("settings") != self.header.get("settings")
        {
            return Ok(());
        }
        let present: BTreeSet<&str> = draft.nodes.values().map(|n| n.package.as_str()).collect();
        let historical: BTreeSet<String> = graph::section(self.old, "snapshots")
            .into_iter()
            .flatten()
            .map(|(key, _)| key)
            .filter(|key| {
                !present.contains(graph::package_key(key))
                    && !self.pinned_snapshots.contains(key.as_str())
                    && !self.reopened.contains(*key)
                    && RegistryPackage::from_key(key).is_some()
            })
            .cloned()
            .collect();
        self.pending.extend(historical);
        self.expand(draft)
    }

    /// Hooks have already run on the complete manifests of these ordinary releases.
    /// Retain a historical peer instance only if its evaluated declarations resolve
    /// to the same children and its importer still supplies the same peers.
    pub fn retain_evaluated(&mut self, draft: &mut Draft) -> Result<Option<Value>> {
        if !self.cache.reads_packages() || self.old.get("settings") != self.header.get("settings") {
            return Ok(None);
        }
        if self.seeded_history {
            self.retain_evaluated_subgraph(draft)?;
            return Ok(None);
        }
        let snapshots = graph::section(self.old, "snapshots");
        let mut releases = BTreeMap::<&str, Vec<&Rc<Node>>>::new();
        for node in draft.nodes.values() {
            releases.entry(&node.package).or_default().push(node);
        }
        let mut witnessed = BTreeMap::new();
        let dirty = self.base.partition.incomplete(|key, body| {
            let package = graph::package_key(key);
            if self.reopened.contains(key) || self.local_dirs.contains_key(package) {
                return true;
            }
            if let Some(node) = draft.nodes.get(key) {
                return !*node
                    .historical_edges_match
                    .get_or_init(|| node.matches_history(self.old, key, body, self.config, true));
            }
            // Ordinary expansion does not visit every peer-only occurrence.
            // Reuse one only if the same publication has already run its hook,
            // and its selected ordinary releases and patch match that occurrence.
            if !self.pinned_snapshots.contains(key)
                && let Some(node) = releases
                    .get(package)
                    .into_iter()
                    .flatten()
                    .find(|n| n.matches_history(self.old, key, body, self.config, false))
            {
                witnessed.insert(key.to_owned(), Rc::clone(node));
                return false;
            }
            true
        });
        self.base.partition.apply(draft, &dirty);
        // A changed parent can still reach an unchanged child instance. The child's
        // evaluated publication supplies current metadata even when its edges survive.
        for (key, body) in snapshots.into_iter().flatten() {
            if !dirty.contains(key) {
                let node = draft
                    .nodes
                    .get(key)
                    .or_else(|| witnessed.get(key))
                    .expect("validated publication");
                draft.document["packages"][&node.package] = node.record.clone();
                draft.document["snapshots"][key] = body.clone();
            }
        }
        if draft
            .roots
            .values()
            .flat_map(|deps| deps.values())
            .all(|dep| dep.locked || dep.target.starts_with("link:"))
        {
            graph::prune(&mut draft.document, crate::DocumentKind::Project)?;
            graph::refresh_optional(&mut draft.document)?;
            project_times(&mut draft.document, self.old, self.config);
            return Ok(Some(std::mem::take(&mut draft.document)));
        }
        Ok(None)
    }

    fn retain_evaluated_subgraph(&mut self, draft: &mut Draft) -> Result<()> {
        loop {
            let dirty = self
                .base
                .partition
                .incomplete(|key, _| draft.document["snapshots"].get(key).is_none());
            self.base.partition.apply(draft, &dirty);
            self.pending.extend(
                draft
                    .roots
                    .values()
                    .flat_map(|deps| deps.values())
                    .filter(|dep| {
                        !dep.locked
                            && !dep.target.starts_with("link:")
                            && !draft.nodes.contains_key(&dep.target)
                    })
                    .map(|dep| dep.target.clone()),
            );
            if self.pending.is_empty() {
                if let Some(records) = draft.document["snapshots"].as_object_mut() {
                    for key in dirty {
                        records.remove(&key);
                    }
                }
                return Ok(());
            }
            self.expand(draft)?;
        }
    }
}
