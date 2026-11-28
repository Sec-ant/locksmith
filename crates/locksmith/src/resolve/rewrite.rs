//! Apply final overrides, extensions and patches to a selected snapshot.
use super::{GraphSources, PeerChanges, PeerRequirements, RepairedSnapshot, peer_names};
use crate::{
    LockfileError, Value, model::catalog, model::error::missing, model::graph, model::manifest,
    model::pin, peer,
};
use serde_json::json;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

struct SnapshotProviders<'a> {
    source: Option<(usize, &'a str)>,
    providers: BTreeMap<String, String>,
}

impl SnapshotProviders<'_> {
    fn resolve(&mut self, peers: &mut peer::Merger<'_>) -> &mut BTreeMap<String, String> {
        // Recovering an opaque peer identity can require a hash search. Edges
        // that retain their locked references never need the enclosing scope.
        if let Some((side, key)) = self.source.take()
            && let Some(inherited) = peers.binding_context(side, key, &BTreeMap::new())
        {
            for (name, value) in inherited {
                if name != "patch_hash" {
                    self.providers.entry(name).or_insert(value);
                }
            }
        }
        &mut self.providers
    }
}

fn repair_peer_declarations(
    key: &str,
    side: usize,
    package: &Value,
    body: &Value,
    sources: &GraphSources<'_>,
) -> Result<(Value, BTreeSet<String>), LockfileError> {
    let mut result = package.clone();
    let mut changed = BTreeSet::new();
    if sources.inputs[side].unchanged_rules {
        return Ok((result, changed));
    }
    let Some((parent_name, parent_version)) = key.rsplit_once('@') else {
        return Ok((result, changed));
    };
    let parent = json!({"name":parent_name,"version":parent_version});
    for (name, declaration) in graph::section(package, "peerDependencies")
        .into_iter()
        .flatten()
    {
        let before = sources.inputs[side]
            .rules
            .relevant(name, parent_name, parent_version);
        let after = sources
            .final_rules
            .relevant(name, parent_name, parent_version);
        if before == after
            && (after.len() <= 1 || sources.inputs[side].tie_break == sources.override_tie_break)
        {
            continue;
        }
        let declaration = declaration.as_str().expect("peer declaration");
        // A removed peer that was never installed has no edge or binding to
        // replace. Keep the instance and retire the manifest declaration.
        if sources
            .final_rules
            .apply(&parent, ".", name, declaration)?
            .is_none()
            && graph::dependency(body, name).is_none()
            && (before.is_empty()
                || after
                    .iter()
                    .all(|(rule, _)| manifest::override_selector(rule).2.is_none()))
        {
            for field in ["peerDependencies", "peerDependenciesMeta"] {
                if let Some(entries) = result.get_mut(field).and_then(Value::as_object_mut) {
                    entries.remove(name);
                    if entries.is_empty() {
                        result.as_object_mut().unwrap().remove(field);
                    }
                }
            }
            changed.insert(name.clone());
            continue;
        }
        let witnessed = sources.order.iter().find_map(|source| {
            let candidate = sources.inputs[*source].document.get("packages")?.get(key)?;
            (sources.inputs[*source].compatibility.accepts(key)
                && candidate.get("resolution") == package.get("resolution")
                && sources.inputs[*source]
                    .rules
                    .relevant(name, parent_name, parent_version)
                    == after
                && (after.len() <= 1
                    || sources.inputs[*source].tie_break == sources.override_tie_break))
                .then(|| candidate.get("peerDependencies")?.get(name)?.as_str())
                .flatten()
        });
        let expected = if let Some(witnessed) = witnessed {
            Cow::Borrowed(witnessed)
        } else if before.is_empty()
            || (!after.is_empty()
                && after
                    .iter()
                    .all(|(rule, _)| manifest::override_selector(rule).2.is_none()))
        {
            sources
                .final_rules
                .apply(&parent, ".", name, declaration)?
                .ok_or_else(|| {
                    missing(
                        &["packages", key, "peerDependencies", name],
                        "removing a peer override requires the original dependency declarations",
                    )
                })?
        } else {
            let original = super::published::original(
                sources,
                package,
                key,
                "peerDependencies",
                name,
                "original peer declaration is required to evaluate changed overrides",
            )?
            .ok_or_else(|| {
                missing(
                    &["packages", key, "peerDependencies", name],
                    "published release has no matching peer declaration",
                )
            })?;
            Cow::Owned(
                sources
                    .final_rules
                    .apply(&parent, ".", name, &original)?
                    .ok_or_else(|| {
                        missing(
                            &["packages", key, "peerDependencies", name],
                            "final override removes the peer declaration",
                        )
                    })?
                    .into_owned(),
            )
        };
        if expected != declaration {
            if expected.parse::<node_semver::Range>().is_err() {
                return Err(missing(
                    &["packages", key, "peerDependencies", name],
                    "non-range peer overrides require ordinary dependency metadata",
                ));
            }
            result["peerDependencies"][name] = Value::String(expected.into_owned());
            changed.insert(name.clone());
        }
    }
    Ok((result, changed))
}

fn extend_dependencies(
    key: &str,
    side: usize,
    result: &mut Value,
    previous: &mut SnapshotProviders<'_>,
    metadata: &Value,
    sources: &GraphSources<'_>,
    peers: &mut peer::Merger<'_>,
) -> Result<(BTreeSet<String>, PeerRequirements), LockfileError> {
    let mut changed = BTreeSet::new();
    let mut requirements = PeerRequirements::new();
    let additions = sources.inputs[side].compatibility.additions(key);
    if additions.is_empty() {
        return Ok((changed, requirements));
    }
    let previous = previous.resolve(peers);
    let package_key = pin::package_key(key);
    let (parent_name, parent_version) = package_key.rsplit_once('@').expect("package identity");
    let parent = json!({"name":parent_name,"version":parent_version});
    let package = &sources.inputs[side].document["packages"][package_key];
    let ignores_optional = sources.final_rules.optional_exclusion()?;
    for addition in additions {
        let crate::model::config::ExtensionAddition {
            group,
            name,
            specifier,
        } = *addition;
        if group == "optionalDependencies" && ignores_optional(name) {
            continue;
        }
        let Some(desired) = sources.final_rules.apply(&parent, ".", name, specifier)? else {
            continue;
        };
        if package
            .get("peerDependencies")
            .and_then(|m| m.get(name))
            .is_some()
            || sources.inputs[side]
                .rules
                .relevant(name, parent_name, parent_version)
                .iter()
                .any(|(_, replacement)| replacement.as_str() == Some("-"))
        {
            return Err(missing(
                &["snapshots", key, name],
                "adding an extension to a peer or previously removed dependency requires its original declaration",
            ));
        }
        if result.get(group).and_then(|m| m.get(name)).is_some()
            || (group == "dependencies"
                && result
                    .get("optionalDependencies")
                    .and_then(|m| m.get(name))
                    .is_some())
        {
            continue;
        }
        let (actual_name, _) = sources.configuration.registry_request(name, &desired);
        let mut selected = None;
        for existing_patch in [true, false] {
            for (candidate_side, candidate) in
                sources.candidates().get(actual_name).into_iter().flatten()
            {
                let candidate_ref = pin::reference(name, candidate);
                if !catalog::accepts_pin(
                    name,
                    &desired,
                    &candidate_ref,
                    metadata,
                    Some(sources.configuration),
                ) || sources.authorized_patch(candidate, metadata) != existing_patch
                {
                    continue;
                }
                let Ok(hash) = sources.desired_patch(candidate, metadata) else {
                    continue;
                };
                let Some(bindings) = peers.source_context(*candidate_side, candidate, previous)
                else {
                    continue;
                };
                let Some(resolved) = peers
                    .recontextualize_source(*candidate_side, candidate, &bindings, previous)
                    .and_then(|key| peers.repatch(*candidate_side, &key, hash))
                else {
                    continue;
                };
                let required = bindings
                    .into_keys()
                    .filter(|name| name != "patch_hash")
                    .collect::<BTreeSet<_>>();
                selected = Some((pin::reference(name, &resolved), required));
                break;
            }
            if selected.is_some() {
                break;
            }
        }
        let (reference, required) = selected.ok_or_else(|| {
            missing(
                &["snapshots", key, name],
                format!("no locked resolution satisfies extension {desired}"),
            )
        })?;
        if group == "optionalDependencies"
            && let Some(dependencies) = result
                .get_mut("dependencies")
                .and_then(Value::as_object_mut)
        {
            dependencies.remove(name);
            if dependencies.is_empty() {
                result
                    .as_object_mut()
                    .expect("snapshot")
                    .remove("dependencies");
            }
        }
        if result.get(group).is_none() {
            result[group] = json!({});
        }
        result[group][name] = json!(reference);
        previous.insert(name.into(), reference);
        changed.insert(name.into());
        if !required.is_empty() {
            requirements.insert(name.into(), required);
        }
    }
    Ok((changed, requirements))
}

pub(super) fn repair_snapshot(
    key: &str,
    side: usize,
    body: &Value,
    package: &Value,
    metadata: &Value,
    sources: &GraphSources<'_>,
    peers: &mut peer::Merger<'_>,
) -> Result<RepairedSnapshot, LockfileError> {
    let (package, changed_peers) =
        repair_peer_declarations(pin::package_key(key), side, package, body, sources)?;
    let unchanged = |package| RepairedSnapshot {
        body: body.clone(),
        package,
        peers: PeerChanges::default(),
        extended: false,
    };
    if snapshot_materialized(side, body, sources, peers) && changed_peers.is_empty() {
        return Ok(unchanged(package));
    }
    let mut previous = snapshot_providers(key, side, body)?;
    let Some((parent_name, parent_version)) = pin::package_key(key).rsplit_once('@') else {
        return Ok(unchanged(package));
    };
    let retired: BTreeSet<_> = changed_peers
        .iter()
        .filter(|name| package["peerDependencies"].get(*name).is_none())
        .cloned()
        .collect();
    let repair = SnapshotRepair {
        key,
        side,
        package: &package,
        metadata,
        sources,
        parent_name,
        parent_version,
        parent: json!({"name": parent_name, "version": parent_version}),
    };
    let mut result = body.clone();
    let (extended, added_peers) = extend_dependencies(
        key,
        side,
        &mut result,
        &mut previous,
        metadata,
        sources,
        peers,
    )?;
    let mut changes = DependencyChanges {
        changed: extended.clone(),
        added_peers,
        changed_peers,
        ..DependencyChanges::default()
    };
    let ignores_optional = sources.final_rules.optional_exclusion()?;
    for group in SNAPSHOT_DEPENDENCIES {
        for (name, value) in graph::section(body, group).into_iter().flatten() {
            if extended.contains(name) {
                continue;
            }
            let reference = value.as_str().expect("checked reference");
            repair.update_edge(
                SnapshotEdge {
                    group,
                    name,
                    reference,
                },
                &mut result,
                &mut previous,
                &mut changes,
                &ignores_optional,
                peers,
            )?;
        }
    }
    let changes =
        repair.finish_peer_changes(&result, &mut previous, changes, &extended, retired, peers)?;
    Ok(RepairedSnapshot {
        body: result,
        package,
        peers: changes,
        extended: !extended.is_empty(),
    })
}

const SNAPSHOT_DEPENDENCIES: [&str; 2] = ["dependencies", "optionalDependencies"];

fn snapshot_materialized(
    side: usize,
    body: &Value,
    sources: &GraphSources<'_>,
    peers: &peer::Merger<'_>,
) -> bool {
    let source_snapshots =
        graph::section(sources.inputs[side].document, "snapshots").expect("source snapshots");
    sources.inputs[side].unchanged_rules
        && sources.inputs[side].matches_configuration
        && SNAPSHOT_DEPENDENCIES.into_iter().all(|group| {
            graph::section(body, group)
                .into_iter()
                .flatten()
                .all(|(name, value)| {
                    let Some(reference) = value.as_str() else {
                        return false;
                    };
                    if reference.starts_with("link:") {
                        return true;
                    }
                    let key = graph::snapshot_key(name, reference, Some(source_snapshots));
                    source_snapshots.contains_key(&key) || peers.has_generated(&key)
                })
        })
}

fn snapshot_providers<'a>(
    key: &'a str,
    side: usize,
    body: &Value,
) -> Result<SnapshotProviders<'a>, LockfileError> {
    let mut previous = SnapshotProviders {
        source: Some((side, key)),
        providers: BTreeMap::new(),
    };
    for group in SNAPSHOT_DEPENDENCIES {
        for (name, value) in graph::section(body, group).into_iter().flatten() {
            let reference = value
                .as_str()
                .ok_or_else(|| missing(&["snapshots", key, name], "expected reference"))?;
            previous.providers.insert(name.clone(), reference.into());
        }
    }
    Ok(previous)
}

struct SnapshotRepair<'a, 'graph> {
    key: &'a str,
    side: usize,
    package: &'a Value,
    metadata: &'a Value,
    sources: &'a GraphSources<'graph>,
    parent_name: &'a str,
    parent_version: &'a str,
    parent: Value,
}

#[derive(Clone, Copy)]
struct SnapshotEdge<'a> {
    group: &'a str,
    name: &'a str,
    reference: &'a str,
}

#[derive(Default)]
struct DependencyChanges {
    changed: BTreeSet<String>,
    changed_peers: BTreeSet<String>,
    removed: BTreeSet<String>,
    unresolved: BTreeSet<String>,
    added_peers: PeerRequirements,
    changed_contexts: PeerRequirements,
}

#[derive(Clone, Copy)]
enum ReplacementMode<'a> {
    Override(&'a str),
    PatchOnly,
    SameRelease,
}

struct ReplacementCandidate<'a> {
    side: usize,
    key: &'a str,
    hash: Option<&'a str>,
    bindings: BTreeMap<String, String>,
    affected: BTreeSet<String>,
    required: BTreeSet<String>,
}

impl ReplacementCandidate<'_> {
    fn rebind(
        self,
        name: &str,
        previous: &BTreeMap<String, String>,
        changes: &mut DependencyChanges,
        peers: &mut peer::Merger<'_>,
    ) -> Option<String> {
        let key = peers.recontextualize_source(self.side, self.key, &self.bindings, previous)?;
        let patched = peers.repatch(self.side, &key, self.hash)?;
        if !self.affected.is_empty() {
            changes.changed_contexts.insert(name.into(), self.affected);
        }
        if !self.required.is_empty() {
            changes.added_peers.insert(name.into(), self.required);
        }
        Some(pin::reference(name, &patched))
    }
}

impl SnapshotRepair<'_, '_> {
    fn expected<'a>(
        &'a self,
        edge: SnapshotEdge<'a>,
        own_peer: bool,
        peer_changed: bool,
        ignores_optional: &impl Fn(&str) -> bool,
    ) -> Result<Option<Cow<'a, str>>, LockfileError> {
        let Self {
            key,
            side,
            package,
            metadata,
            sources,
            parent_name,
            parent_version,
            ..
        } = *self;
        let SnapshotEdge {
            group,
            name,
            reference,
        } = edge;
        let expected = if peer_changed {
            Some(Cow::Borrowed(
                package["peerDependencies"][name]
                    .as_str()
                    .expect("peer declaration"),
            ))
        } else if own_peer {
            Some(Cow::Borrowed(pin::package_key(reference)))
        } else {
            sources
                .unqualified_rules
                .apply(&self.parent, ".", name, pin::package_key(reference))?
        };
        if own_peer {
            return Ok(expected);
        }
        let before = sources.inputs[side]
            .rules
            .relevant(name, parent_name, parent_version);
        let after = sources
            .final_rules
            .relevant(name, parent_name, parent_version);
        if before == after
            && (after.len() <= 1 || sources.inputs[side].tie_break == sources.override_tie_break)
        {
            return Ok(expected);
        }
        // The same package release in another peer context can
        // witness this dependency under the final overrides.
        // This reuses a proven resolution without inferring its
        // original declaration from its locked version.
        let mut witnesses = sources
            .candidates()
            .get(parent_name)
            .into_iter()
            .flatten()
            .filter(|(source, candidate)| {
                parent_version.parse::<node_semver::Version>().is_ok()
                    && pin::package_key(candidate) == pin::package_key(key)
                    && sources.inputs[*source]
                        .document
                        .get("packages")
                        .and_then(|v| v.get(pin::package_key(candidate)))
                        .and_then(|v| v.get("resolution"))
                        == package.get("resolution")
                    && sources.authorized_patch(candidate, metadata)
                    && (after.len() <= 1
                        || sources.inputs[*source].tie_break == sources.override_tie_break)
                    && sources.inputs[*source]
                        .rules
                        .relevant(name, parent_name, parent_version)
                        == after
            });
        let locked_reference = witnesses.clone().find_map(|(source, candidate)| {
            sources.inputs[*source]
                .document
                .get("snapshots")?
                .get(*candidate)?
                .get(group)?
                .get(name)?
                .as_str()
        });
        if let Some(reference) = locked_reference {
            let version = pin::package_key(reference);
            return Ok(Some(if version.rsplit_once('@').is_some() {
                Cow::Owned(format!("npm:{version}"))
            } else {
                Cow::Borrowed(version)
            }));
        }
        if let Some(declaration) = witnesses.find_map(|(source, candidate)| {
            let addition = sources.inputs[*source]
                .compatibility
                .additions(candidate)
                .iter()
                .find(|addition| addition.group == group && addition.name == name)?;
            let body = &sources.inputs[*source].document["snapshots"][*candidate];
            if sources.inputs[*source].document["packages"][pin::package_key(candidate)]
                .get("peerDependencies")
                .is_some_and(|peers| peers.get(name).is_some())
                || (group == "dependencies"
                    && body
                        .get("optionalDependencies")
                        .is_some_and(|deps| deps.get(name).is_some()))
                || (group == "optionalDependencies" && ignores_optional(name))
            {
                return None;
            }
            Some(addition.specifier)
        }) {
            // The final extension supplies a declaration when an
            // unmodified source proves the package did not declare
            // this dependency itself. Existing locked edges win.
            return sources
                .final_rules
                .apply(&self.parent, ".", name, declaration);
        }
        let reason = if before
            .iter()
            .chain(&after)
            .any(|(rule, _)| manifest::override_selector(rule).2.is_some())
        {
            "original dependency declaration is required to evaluate a changed qualified override"
        } else if !before.is_empty() && after.is_empty() {
            "original dependency declaration is required after removing an override"
        } else {
            return Ok(expected);
        };
        let Some(declared) =
            super::published::original(sources, package, key, group, name, reason)?
        else {
            return Ok(None);
        };
        Ok(sources
            .final_rules
            .apply(&self.parent, ".", name, &declared)?
            .map(|value| Cow::Owned(value.into_owned())))
    }

    fn update_edge(
        &self,
        edge: SnapshotEdge<'_>,
        result: &mut Value,
        previous: &mut SnapshotProviders<'_>,
        changes: &mut DependencyChanges,
        ignores_optional: &impl Fn(&str) -> bool,
        peers: &mut peer::Merger<'_>,
    ) -> Result<(), LockfileError> {
        let Self {
            key,
            package,
            metadata,
            sources,
            ..
        } = *self;
        let SnapshotEdge {
            group,
            name,
            reference,
        } = edge;
        let old_key = peers.snapshot_key(name, reference);
        let own_peer = package
            .get("peerDependencies")
            .and_then(|v| v.get(name))
            .is_some();
        let mut expected = self.expected(
            edge,
            own_peer,
            changes.changed_peers.contains(name),
            ignores_optional,
        )?;
        let mut needs_override = expected.as_ref().is_none_or(|range| {
            range.as_ref() != pin::package_key(reference)
                && !catalog::accepts_pin(
                    name,
                    range,
                    reference,
                    metadata,
                    Some(sources.configuration),
                )
        });
        let needs_patch =
            !reference.starts_with("link:") && !sources.authorized_patch(&old_key, metadata);
        let needs_configuration = !reference.starts_with("link:")
            && peers.origin_side(&old_key).map_or_else(
                || {
                    !sources.order.iter().any(|source| {
                        sources.inputs[*source].compatibility.unchanged(&old_key)
                            && sources.inputs[*source]
                                .document
                                .get("snapshots")
                                .and_then(|records| records.get(&old_key))
                                .is_some()
                    })
                },
                |source| !sources.inputs[source].compatibility.unchanged(&old_key),
            );
        if own_peer
            && needs_configuration
            && !pin::package_key(&old_key)
                .rsplit_once('@')
                .is_some_and(|(name, _)| {
                    sources
                        .candidates()
                        .get(name)
                        .into_iter()
                        .flatten()
                        .any(|(_, candidate)| {
                            pin::package_key(candidate) == pin::package_key(&old_key)
                                && sources.authorized_patch(candidate, metadata)
                        })
                })
            && let Some(declaration) = package["peerDependencies"][name].as_str()
            && !catalog::accepts_pin(
                name,
                declaration,
                reference,
                metadata,
                Some(sources.configuration),
            )
        {
            expected = Some(Cow::Borrowed(declaration));
            needs_override = true;
            changes.changed_peers.insert(name.into());
        }
        if !needs_override && !needs_patch && !needs_configuration {
            return Ok(());
        }
        let Some(expected) = expected else {
            result[group].as_object_mut().unwrap().remove(name);
            changes.removed.insert(name.into());
            return Ok(());
        };
        let (old_name, old_version) = pin::package_key(&old_key)
            .rsplit_once('@')
            .ok_or_else(|| missing(&["snapshots", key, name], "missing package identity"))?;
        let desired = if needs_override {
            expected.as_ref()
        } else {
            old_version
        };
        let dependency = if needs_override { name } else { old_name };
        let (actual_name, _) = sources.configuration.registry_request(dependency, desired);
        let mode = if needs_override {
            ReplacementMode::Override(desired)
        } else if needs_patch && !needs_configuration {
            ReplacementMode::PatchOnly
        } else {
            ReplacementMode::SameRelease
        };
        let selected = self.select_replacement(
            name,
            &old_key,
            actual_name,
            mode,
            previous.resolve(peers),
            changes,
            peers,
        );
        if selected.is_none()
            && changes.changed_peers.contains(name)
            && !needs_patch
            && !needs_configuration
        {
            // An explicit enclosing provider may intentionally violate a
            // peer range. Resolve it in the workspace scope before requiring
            // a newly auto-installed provider from the locked candidates.
            changes.unresolved.insert(name.into());
            changes.changed.insert(name.into());
            return Ok(());
        }
        let selected = selected.ok_or_else(|| {
            super::published::need_version(
                &["snapshots", key, name],
                actual_name,
                desired,
                sources.configuration,
                format!("no compatible locked instance satisfies override/patch {desired}"),
            )
        })?;
        result[group][name] = Value::String(selected);
        changes.changed.insert(name.into());
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn select_replacement(
        &self,
        name: &str,
        old_key: &str,
        actual_name: &str,
        mode: ReplacementMode<'_>,
        previous: &BTreeMap<String, String>,
        changes: &mut DependencyChanges,
        peers: &mut peer::Merger<'_>,
    ) -> Option<String> {
        let Self {
            side,
            metadata,
            sources,
            ..
        } = *self;
        let mut providers = previous.clone();
        if let Some(bindings) = peers.binding_context(side, old_key, previous) {
            for (name, binding) in bindings {
                providers.entry(name).or_insert(binding);
            }
        }
        providers.remove("patch_hash");
        let canonical = peer::canonical_providers(&providers);
        let repatch = |peers: &mut peer::Merger<'_>, before_search| {
            if !matches!(mode, ReplacementMode::PatchOnly) {
                return None;
            }
            let hash = sources.desired_patch(old_key, metadata).ok()?;
            if before_search && !sources.prefer_baseline && hash.is_some() {
                return None;
            }
            let source = peers.origin_side(old_key).unwrap_or(side);
            peers
                .repatch(source, old_key, hash)
                .map(|key| pin::reference(name, &key))
        };
        // Patch-only changes can preserve the locked artifact and edges.
        if let Some(selected) = repatch(peers, true) {
            return Some(selected);
        }
        // Prefer a proven final-patch graph before deriving a new one.
        for reconstructed in [false, true] {
            for existing_patch in [true, false] {
                if !existing_patch && let Some(selected) = repatch(peers, false) {
                    return Some(selected);
                }
                let mut deferred = Vec::new();
                for &(side, key) in sources.candidates().get(actual_name).into_iter().flatten() {
                    let matches = match mode {
                        ReplacementMode::Override(desired) => catalog::accepts_pin(
                            name,
                            desired,
                            &pin::reference(name, key),
                            metadata,
                            Some(sources.configuration),
                        ),
                        ReplacementMode::SameRelease | ReplacementMode::PatchOnly => {
                            pin::package_key(key) == pin::package_key(old_key)
                        }
                    };
                    if sources.inputs[side].compatibility.unchanged(key) == reconstructed
                        || !matches
                    {
                        continue;
                    }
                    let Ok(hash) = sources.desired_patch(key, metadata) else {
                        continue;
                    };
                    if (hash == crate::model::patch::hash_in_key(key)) != existing_patch {
                        continue;
                    }
                    let Some(bindings) = peers.source_context(side, key, previous) else {
                        continue;
                    };
                    let affected = bindings
                        .iter()
                        .filter(|(name, value)| {
                            name.as_str() != "patch_hash" && canonical.get(*name) != Some(*value)
                        })
                        .map(|(name, _)| name.clone())
                        .collect();
                    let required = bindings
                        .keys()
                        .filter(|name| {
                            name.as_str() != "patch_hash" && !canonical.contains_key(*name)
                        })
                        .cloned()
                        .collect();
                    let candidate = ReplacementCandidate {
                        side,
                        key,
                        hash,
                        bindings,
                        affected,
                        required,
                    };
                    if candidate.required.is_empty() {
                        if let Some(reference) = candidate.rebind(name, previous, changes, peers) {
                            return Some(reference);
                        }
                    } else {
                        deferred.push(candidate);
                    }
                }
                for candidate in deferred {
                    if let Some(reference) = candidate.rebind(name, previous, changes, peers) {
                        return Some(reference);
                    }
                }
            }
        }
        None
    }

    fn finish_peer_changes(
        &self,
        result: &Value,
        previous: &mut SnapshotProviders<'_>,
        changes: DependencyChanges,
        extended: &BTreeSet<String>,
        retired: BTreeSet<String>,
        peers: &mut peer::Merger<'_>,
    ) -> Result<PeerChanges, LockfileError> {
        let Self {
            key,
            package,
            parent_name,
            ..
        } = *self;
        let DependencyChanges {
            changed,
            removed,
            unresolved,
            added_peers,
            changed_contexts,
            ..
        } = changes;
        if changed.is_empty() && removed.is_empty() {
            let mut peers = PeerChanges::default();
            if !retired.is_empty() {
                peers.supplied.insert(key.into(), retired);
            }
            return Ok(peers);
        }
        let previous = previous.resolve(peers);
        let mut requirements = PeerRequirements::new();
        let mut affected = self.affected_children(result, previous, &changed, &removed, peers)?;
        requirements.entry(key.into()).or_default().extend(
            changed
                .iter()
                .filter(|name| {
                    package
                        .get("peerDependencies")
                        .is_some_and(|peers| peers.get(*name).is_some())
                })
                .cloned(),
        );
        requirements.retain(|_, names| !names.is_empty());
        for (name, names) in added_peers {
            let reference = SNAPSHOT_DEPENDENCIES
                .into_iter()
                .find_map(|group| result.get(group)?.get(&name)?.as_str())
                .expect("selected dependency");
            requirements
                .entry(peers.snapshot_key(&name, reference))
                .or_default()
                .extend(names);
        }
        for (name, names) in changed_contexts {
            let reference = SNAPSHOT_DEPENDENCIES
                .into_iter()
                .find_map(|group| result.get(group)?.get(&name)?.as_str())
                .expect("selected dependency");
            affected
                .entry(peers.snapshot_key(&name, reference))
                .or_default()
                .extend(names);
        }
        let unresolved = if unresolved.is_empty() {
            PeerRequirements::new()
        } else {
            BTreeMap::from([(key.into(), unresolved)])
        };
        Ok(PeerChanges {
            required: requirements,
            affected,
            unresolved,
            supplied: if extended.is_empty() && retired.is_empty() {
                PeerRequirements::new()
            } else {
                let mut names = extended
                    .iter()
                    .map(|name| peer::canonical_binding(name, &previous[name]).0.to_owned())
                    .collect::<BTreeSet<_>>();
                names.extend(retired);
                names.insert(parent_name.into());
                BTreeMap::from([(key.into(), names)])
            },
        })
    }

    fn affected_children(
        &self,
        result: &Value,
        previous: &BTreeMap<String, String>,
        changed: &BTreeSet<String>,
        removed: &BTreeSet<String>,
        peers: &mut peer::Merger<'_>,
    ) -> Result<PeerRequirements, LockfileError> {
        let Self { key, sources, .. } = *self;
        let mut affected = PeerRequirements::new();
        for group in SNAPSHOT_DEPENDENCIES {
            let entries = graph::section(result, group).into_iter().flatten();
            for (name, value) in entries {
                let reference = value.as_str().expect("selected reference");
                if reference.starts_with("link:") {
                    continue;
                }
                let child = peers.snapshot_key(name, reference);
                let child_side = peers
                    .origin_side(&child)
                    .or_else(|| {
                        sources.order.iter().copied().find(|side| {
                            sources.inputs[*side]
                                .document
                                .get("snapshots")
                                .and_then(|v| v.get(&child))
                                .is_some()
                        })
                    })
                    .ok_or_else(|| missing(&["snapshots", &child], "missing instance"))?;
                let mut names = peer_names(sources.inputs[child_side].document, &child);
                if let Some(bindings) = peers.binding_context(child_side, &child, previous) {
                    names.extend(bindings.into_keys().filter(|name| name != "patch_hash"));
                }
                if !names.is_disjoint(removed) {
                    return Err(missing(
                        &["snapshots", key, name],
                        "removing a local peer provider requires its enclosing provider context",
                    ));
                }
                if names.is_disjoint(changed) {
                    continue;
                }
                affected
                    .entry(child)
                    .or_default()
                    .extend(names.intersection(changed).cloned());
            }
        }
        Ok(affected)
    }
}
