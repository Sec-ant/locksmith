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
    fn incomplete(
        &self,
        cache: &mut Sources<'_>,
        live: Option<&BTreeSet<&str>>,
    ) -> Result<BTreeSet<String>> {
        let mut refresh = BTreeSet::new();
        let patches_changed =
            self.old.get("patchedDependencies") != self.header.get("patchedDependencies");
        for (key, record) in graph::section(self.old, "packages").into_iter().flatten() {
            let locator = manifest::selector(key).1.unwrap_or_default();
            if locator.starts_with("file:") || record["resolution"]["type"] == "directory" {
                refresh.insert(key.as_str());
            } else if let Some((package, version)) = RegistryPackage::from_key(key)
                && live.is_none_or(|keys| keys.contains(key.as_str()))
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

/// Whether a release's recorded peers and snapshot children still satisfy the
/// declarations of its evaluated manifest.
fn declarations_match(
    key: &str,
    manifest: &Value,
    record: &Value,
    bodies: &[(&str, &Value)],
    build: &mut Build<'_, '_>,
) -> Result<bool> {
    let base = build.base;
    let config = build.config;
    let cache = &mut *build.cache;
    let snapshots = graph::section(build.old, "snapshots");
    let mut peers = BTreeMap::new();
    let mut declared = BTreeMap::new();
    for (group, name, spec) in
        super::published_declarations(manifest, key, ".", &base.rules, config)?
    {
        if group == "peerDependencies" {
            peers.insert(
                name,
                (
                    spec,
                    manifest["peerDependenciesMeta"][name]["optional"] == true,
                ),
            );
        } else {
            declared.insert(name, (group, spec));
        }
    }
    // Older pnpm releases omit the peer declaration when the package's own
    // dependency supplies it. Other instances still record the peer's range.
    if !peers::records_fallback_peers(config) {
        peers.retain(|name, _| {
            !declared.contains_key(name) || record["peerDependencies"].get(*name).is_some()
        });
    }
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
    if !recorded_peers.into_iter().eq(peers
        .iter()
        .map(|(&name, (range, optional))| (name, (Some(range.as_ref()), *optional))))
    {
        return Ok(false);
    }
    if !bodies.iter().all(|(_, body)| {
        ["dependencies", "optionalDependencies"]
            .into_iter()
            .flat_map(|group| graph::section(body, group).into_iter().flatten())
            .all(|(child, _)| {
                declared.contains_key(child.as_str())
                    || peers.contains_key(child.as_str())
                    || record["peerDependencies"].get(child).is_some()
            })
    }) {
        return Ok(false);
    }
    if bodies.is_empty() {
        return Ok(true);
    }
    let mut external = BTreeSet::new();
    if declared.keys().any(|name| peers.contains_key(name))
        && let Some(peers) = graph::section(record, "peerDependencies")
    {
        for &(key, body) in bodies {
            let (bound, _) = external_peers(build.old, key, body, peers, config);
            external.extend(bound.into_iter().map(|name| (key, name)));
        }
    }
    for (dependency, (group, spec)) in declared {
        let mut pins = bodies
            .iter()
            .filter(|(key, _)| !external.contains(&(*key, dependency)))
            .map(|(_, body)| {
                graph::dependency(body, dependency)
                    .filter(|(recorded_group, _)| *recorded_group == group)
                    .map(|(_, pin)| pin)
            })
            .peekable();
        if pins.peek().is_none() {
            continue;
        }
        if spec.starts_with("workspace:") {
            let resolved = base
                .links
                .resolve_for_generation(dependency, &spec, ".", manifest, false)?;
            let Some((target, member)) = resolved else {
                return Ok(false);
            };
            let path = target.strip_prefix("link:").expect("workspace link");
            if super::inject_workspace_dependency(dependency, manifest, &spec, config) {
                let Some(name) = member.and_then(|member| member["name"].as_str()) else {
                    return Ok(false);
                };
                let key = format!("{name}@file:{path}");
                if !pins.all(|pin| {
                    pin.is_some_and(|pin| {
                        let child = graph::snapshot_key(dependency, pin, snapshots);
                        graph::package_key(&child) == key
                    })
                }) {
                    return Ok(false);
                }
            } else if !pins.all(|pin| {
                pin.and_then(|pin| pin.strip_prefix("link:"))
                    .is_some_and(|linked| graph::linked_importer(".", linked) == path)
            }) {
                return Ok(false);
            }
        } else if let Some((package, selector)) = cache.registry_selector(dependency, &spec)? {
            let range = selector.parse::<node_semver::Range>().ok();
            let tag_version = if range.is_none() {
                cache
                    .versions(&package, &selector, false)?
                    .first()
                    .map(String::as_str)
            } else {
                None
            };
            if !pins.all(|pin| {
                let Some(pin) = pin else { return false };
                let child = graph::snapshot_key(dependency, pin, snapshots);
                RegistryPackage::from_key(&child).is_some_and(|(locked, version)| {
                    locked == package
                        && (selector == "*"
                            || tag_version == Some(version)
                            || range.as_ref().is_some_and(|range| {
                                version
                                    .parse::<node_semver::Version>()
                                    .is_ok_and(|v| v.satisfies(range))
                            }))
                })
            }) {
                return Ok(false);
            }
        } else if !pins.all(|pin| {
            let Some(pin) = pin else { return false };
            let child = graph::snapshot_key(dependency, pin, snapshots);
            let locator = if pin.starts_with("link:") {
                pin
            } else {
                manifest::selector(graph::package_key(&child))
                    .1
                    .unwrap_or_default()
            };
            spec == locator
                || ["file:", "link:"].into_iter().any(|protocol| {
                    spec.strip_prefix(protocol)
                        .zip(locator.strip_prefix(protocol))
                        .is_some_and(|(path, locked)| {
                            !path.starts_with('/')
                                && path.as_bytes().get(1) != Some(&b':')
                                && graph::linked_importer(".", path) == locked
                        })
                })
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

impl<'a> Build<'a, '_> {
    /// Evaluate each historical parent before following its recorded children.
    /// Expansion selects fresh children when the parent can no longer be kept.
    fn evaluate_history(
        &mut self,
        draft: &mut Draft,
    ) -> Result<(BTreeSet<&'a str>, BTreeSet<&'a str>)> {
        let mut stale = BTreeSet::new();
        let mut live = BTreeSet::new();
        let Some(snapshots) = graph::section(self.old, "snapshots") else {
            return Ok((live, stale));
        };
        let mut pending: Vec<_> = draft
            .roots
            .values()
            .flat_map(|deps| deps.values())
            .filter_map(|dep| snapshots.get_key_value(&dep.target))
            .collect();
        let mut seen = BTreeSet::new();
        while let Some((key, body)) = pending.pop() {
            if !seen.insert(key.as_str()) {
                continue;
            }
            let base = graph::package_key(key);
            let Some(record) = self.old["packages"].get(base) else {
                continue;
            };
            if stale.contains(base) {
                continue;
            }
            let locator = manifest::selector(base).1.unwrap_or_default();
            let release = if RegistryPackage::from_key(base).is_some() {
                Some(self.cache.release(base)?)
            } else if record["resolution"]["type"] == "git"
                || locator.starts_with("http://")
                || locator.starts_with("https://")
            {
                Some(self.cache.locked_source(base, &record["resolution"])?)
            } else if locator.starts_with("file:") || record["resolution"]["type"] == "directory" {
                continue;
            } else {
                None
            };
            if let Some(release) = release {
                let manifest = &Rc::clone(release).manifest;
                if live.insert(base) {
                    reconcile::published::refresh_installation_metadata(
                        &mut draft.document["packages"][base],
                        manifest,
                    );
                }
                if !declarations_match(base, manifest, record, &[(key, body)], self)? {
                    stale.insert(base);
                    continue;
                }
            }
            for group in ["dependencies", "optionalDependencies"] {
                for (name, reference) in graph::section(body, group).into_iter().flatten() {
                    if let Some(pin) = reference.as_str().filter(|pin| !pin.starts_with("link:"))
                        && let Some(child) = snapshots.get_key_value(&graph::snapshot_key(
                            name,
                            pin,
                            Some(snapshots),
                        ))
                    {
                        pending.push(child);
                    }
                }
            }
        }
        Ok((live, stale))
    }

    /// The lockfile does not record whether the compatibility database was enabled.
    fn stale_compat_releases(&mut self) -> Result<BTreeSet<&'a str>> {
        let old: &'a Value = self.old;
        let snapshots = graph::section(old, "snapshots");
        let mut occurrences = BTreeMap::<&str, Vec<(&str, &Value)>>::new();
        for (snapshot, body) in snapshots.into_iter().flatten() {
            occurrences
                .entry(graph::package_key(snapshot))
                .or_default()
                .push((snapshot, body));
        }
        let mut stale = BTreeSet::new();
        for (key, record) in graph::section(old, "packages").into_iter().flatten() {
            if !self.base.compat_candidates.contains(key.as_str())
                || RegistryPackage::from_key(key).is_none()
            {
                continue;
            }
            let bodies = occurrences.get(key.as_str()).map_or(&[][..], Vec::as_slice);
            let manifest = &Rc::clone(self.cache.release(key)?).manifest;
            if !declarations_match(key, manifest, record, bodies, self)? {
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
        for field in ["packages", "snapshots"] {
            if let Some(v) = self.old.get(field) {
                draft.document[field] = v.clone();
            }
        }
        let (reachable, stale) = if outdated && self.cache.reads_packages() {
            let (live, stale) = self.evaluate_history(draft)?;
            (Some(live), stale)
        } else if outdated {
            (None, self.stale_compat_releases()?)
        } else {
            (None, BTreeSet::new())
        };
        let mut previous = self.base.incomplete.borrow_mut();
        let scoped;
        let incomplete = if let Some(reachable) = &reachable {
            scoped = self.base.incomplete(self.cache, Some(reachable))?;
            &scoped
        } else {
            if previous
                .as_ref()
                .is_none_or(|(epoch, _)| *epoch != self.cache.policy_epoch)
            {
                *previous = Some((
                    self.cache.policy_epoch,
                    self.base.incomplete(self.cache, None)?,
                ));
            }
            &previous.as_ref().expect("current publication policy").1
        };
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
