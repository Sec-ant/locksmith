//! Apply final overrides, extensions and patches to a selected snapshot.
use super::{
    GraphSources, PeerChanges, PeerRequirements, RepairedSnapshot, accepts, alias,
    authorized_patch, desired_patch, key_in_inputs, missing, peer_names, reference,
};
use crate::{LockfileError, Value, graph, manifest, peer};
use serde_json::json;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

fn repair_peer_declarations(
    key: &str,
    side: usize,
    package: &Value,
    body: &Value,
    sources: &GraphSources<'_>,
) -> Result<(Value, BTreeSet<String>), LockfileError> {
    let mut result = package.clone();
    let mut changed = BTreeSet::new();
    if sources.unchanged_rules[side] {
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
        let before = sources.rules[side].relevant(name, parent_name, parent_version);
        let after = sources
            .final_rules
            .relevant(name, parent_name, parent_version);
        if before == after
            && (after.len() <= 1 || sources.source_tie_breaks[side] == sources.override_tie_break)
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
            let candidate = sources.docs[*source].get("packages")?.get(key)?;
            (sources.compatibility[*source].accepts(key)
                && candidate.get("resolution") == package.get("resolution")
                && sources.rules[*source].relevant(name, parent_name, parent_version) == after
                && (after.len() <= 1
                    || sources.source_tie_breaks[*source] == sources.override_tie_break))
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
    previous: &mut BTreeMap<String, String>,
    metadata: &Value,
    sources: &GraphSources<'_>,
    peers: &mut peer::Merger<'_>,
) -> Result<(BTreeSet<String>, PeerRequirements), LockfileError> {
    let mut changed = BTreeSet::new();
    let mut requirements = PeerRequirements::new();
    let additions = sources.compatibility[side].additions(key);
    if additions.is_empty() {
        return Ok((changed, requirements));
    }
    let package_key = graph::package_key(key);
    let (parent_name, parent_version) = package_key.rsplit_once('@').expect("package identity");
    let parent = json!({"name":parent_name,"version":parent_version});
    let package = &sources.docs[side]["packages"][package_key];
    for addition in additions {
        let crate::config::ExtensionAddition {
            group,
            name,
            specifier,
        } = *addition;
        if group == "optionalDependencies"
            && sources
                .configuration
                .resolution
                .ignored_optional_dependencies
                .as_ref()
                .and_then(Value::as_array)
                .is_some_and(|names| names.iter().any(|n| n == name))
        {
            continue;
        }
        let Some(desired) = sources.final_rules.apply(&parent, ".", name, specifier)? else {
            continue;
        };
        if package
            .get("peerDependencies")
            .and_then(|m| m.get(name))
            .is_some()
            || sources.rules[side]
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
        let actual_name = alias(&desired).map_or(name, |(name, _)| name);
        let mut selected = None;
        for existing_patch in [true, false] {
            for (candidate_side, candidate) in
                sources.candidates().get(actual_name).into_iter().flatten()
            {
                let candidate_ref = reference(name, candidate);
                if !accepts(name, &desired, &candidate_ref, metadata)
                    || authorized_patch(candidate, metadata, sources.docs) != existing_patch
                {
                    continue;
                }
                let Some(hash) = desired_patch(candidate, metadata, sources.docs) else {
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
                selected = Some((reference(name, &resolved), required));
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
    let (package, mut changed_peers) =
        repair_peer_declarations(graph::package_key(key), side, package, body, sources)?;
    let retired: BTreeSet<_> = changed_peers
        .iter()
        .filter(|name| package["peerDependencies"].get(*name).is_none())
        .cloned()
        .collect();
    let mut result = body.clone();
    let mut added_peers = PeerRequirements::new();
    let mut changed_contexts = PeerRequirements::new();
    let mut unresolved = BTreeSet::new();
    let source_snapshots =
        graph::section(sources.docs[side], "snapshots").expect("source snapshots");
    let materialized = sources.unchanged_rules[side]
        && sources.unchanged_configuration[side]
        && ["dependencies", "optionalDependencies"]
            .into_iter()
            .all(|group| {
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
                        source_snapshots.contains_key(&key) || peers.generated.contains_key(&key)
                    })
            });
    if materialized && changed_peers.is_empty() {
        return Ok(RepairedSnapshot {
            body: result,
            package,
            peers: PeerChanges::default(),
            extended: false,
        });
    }
    let mut previous = BTreeMap::new();
    if let Some(bindings) = peers.binding_context(side, key, &previous) {
        previous.extend(
            bindings
                .into_iter()
                .filter(|(name, _)| name != "patch_hash"),
        );
    }
    for group in ["dependencies", "optionalDependencies"] {
        for (name, value) in graph::section(body, group).into_iter().flatten() {
            let reference = value
                .as_str()
                .ok_or_else(|| missing(&["snapshots", key, name], "expected reference"))?;
            previous.insert(name.clone(), reference.into());
        }
    }
    let Some((parent_name, parent_version)) = graph::package_key(key).rsplit_once('@') else {
        return Ok(RepairedSnapshot {
            body: result,
            package,
            peers: PeerChanges::default(),
            extended: false,
        });
    };
    let parent = json!({"name":parent_name,"version":parent_version});
    let (extended, extension_peers) = extend_dependencies(
        key,
        side,
        &mut result,
        &mut previous,
        metadata,
        sources,
        peers,
    )?;
    added_peers.extend(extension_peers);
    let mut changed = extended.clone();
    let mut removed = BTreeSet::new();
    let overrides = &sources.unqualified_rules;
    for group in ["dependencies", "optionalDependencies"] {
        for (name, value) in graph::section(body, group).into_iter().flatten() {
            if extended.contains(name) {
                continue;
            }
            let reference = value.as_str().expect("checked reference");
            let old_key = key_in_inputs(name, reference, sources.docs, &peers.generated);
            let own_peer = package
                .get("peerDependencies")
                .and_then(|v| v.get(name))
                .is_some();
            let mut expected = if changed_peers.contains(name) {
                Some(Cow::Borrowed(
                    package["peerDependencies"][name]
                        .as_str()
                        .expect("peer declaration"),
                ))
            } else if own_peer {
                Some(std::borrow::Cow::Borrowed(graph::package_key(reference)))
            } else {
                overrides.apply(&parent, ".", name, graph::package_key(reference))?
            };
            if !own_peer {
                let before = sources.rules[side].relevant(name, parent_name, parent_version);
                let after = sources
                    .final_rules
                    .relevant(name, parent_name, parent_version);
                if before != after
                    || (after.len() > 1
                        && sources.source_tie_breaks[side] != sources.override_tie_break)
                {
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
                                && graph::package_key(candidate) == graph::package_key(key)
                                && sources.docs[*source]
                                    .get("packages")
                                    .and_then(|v| v.get(graph::package_key(candidate)))
                                    .and_then(|v| v.get("resolution"))
                                    == package.get("resolution")
                                && authorized_patch(candidate, metadata, sources.docs)
                                && (after.len() <= 1
                                    || sources.source_tie_breaks[*source]
                                        == sources.override_tie_break)
                                && sources.rules[*source].relevant(
                                    name,
                                    parent_name,
                                    parent_version,
                                ) == after
                        });
                    let witnessed = witnesses.clone().find_map(|(source, candidate)| {
                        sources.docs[*source]
                            .get("snapshots")?
                            .get(*candidate)?
                            .get(group)?
                            .get(name)?
                            .as_str()
                    });
                    if let Some(witnessed) = witnessed {
                        let version = graph::package_key(witnessed);
                        expected = Some(if version.rsplit_once('@').is_some() {
                            std::borrow::Cow::Owned(format!("npm:{version}"))
                        } else {
                            std::borrow::Cow::Borrowed(version)
                        });
                    } else if let Some(declaration) = witnesses.find_map(|(source, candidate)| {
                        let addition = sources.compatibility[*source]
                            .additions(candidate)
                            .iter()
                            .find(|addition| {
                            addition.group == group && addition.name == name
                        })?;
                        let body = &sources.docs[*source]["snapshots"][*candidate];
                        if sources.docs[*source]["packages"][graph::package_key(candidate)]
                            .get("peerDependencies")
                            .is_some_and(|peers| peers.get(name).is_some())
                            || (group == "dependencies"
                                && body
                                    .get("optionalDependencies")
                                    .is_some_and(|deps| deps.get(name).is_some()))
                            || sources
                                .configuration
                                .resolution
                                .ignored_optional_dependencies
                                .as_ref()
                                .and_then(Value::as_array)
                                .is_some_and(|names| names.iter().any(|value| value == name))
                        {
                            return None;
                        }
                        Some(addition.specifier)
                    }) {
                        // The final extension supplies a declaration when an
                        // unmodified source proves the package did not declare
                        // this dependency itself. Existing locked edges win.
                        expected = sources.final_rules.apply(&parent, ".", name, declaration)?;
                    } else {
                        let reason = if before
                            .iter()
                            .chain(after.iter())
                            .any(|(rule, _)| manifest::override_selector(rule).2.is_some())
                        {
                            Some(
                                "original dependency declaration is required to evaluate a changed qualified override",
                            )
                        } else if !before.is_empty() && after.is_empty() {
                            Some(
                                "original dependency declaration is required after removing an override",
                            )
                        } else {
                            None
                        };
                        if let Some(reason) = reason {
                            let original = super::published::original(
                                sources, &package, key, group, name, reason,
                            )?;
                            expected = match original {
                                Some(declared) => sources
                                    .final_rules
                                    .apply(&parent, ".", name, &declared)?
                                    .map(|v| Cow::Owned(v.into_owned())),
                                None => None,
                            };
                        }
                    }
                }
            }
            let mut needs_override = expected.as_ref().is_none_or(|range| {
                range.as_ref() != graph::package_key(reference)
                    && !accepts(name, range, reference, metadata)
            });
            let needs_patch = !reference.starts_with("link:")
                && !authorized_patch(&old_key, metadata, sources.docs);
            let needs_configuration = !reference.starts_with("link:")
                && peers.origin_side(&old_key).map_or_else(
                    || {
                        !sources.order.iter().any(|source| {
                            sources.compatibility[*source].unchanged(&old_key)
                                && sources.docs[*source]
                                    .get("snapshots")
                                    .and_then(|records| records.get(&old_key))
                                    .is_some()
                        })
                    },
                    |source| !sources.compatibility[source].unchanged(&old_key),
                );
            if own_peer
                && needs_configuration
                && !graph::package_key(&old_key)
                    .rsplit_once('@')
                    .is_some_and(|(name, _)| {
                        sources.candidates().get(name).into_iter().flatten().any(
                            |(_, candidate)| {
                                graph::package_key(candidate) == graph::package_key(&old_key)
                                    && authorized_patch(candidate, metadata, sources.docs)
                            },
                        )
                    })
                && let Some(declaration) = package["peerDependencies"][name].as_str()
                && !accepts(name, declaration, reference, metadata)
            {
                expected = Some(Cow::Borrowed(declaration));
                needs_override = true;
                changed_peers.insert(name.clone());
            }
            if !needs_override && !needs_patch && !needs_configuration {
                continue;
            }
            let Some(expected) = expected else {
                result[group].as_object_mut().unwrap().remove(name);
                removed.insert(name.clone());
                continue;
            };
            let (old_name, old_version) = graph::package_key(&old_key)
                .rsplit_once('@')
                .ok_or_else(|| missing(&["snapshots", key, name], "missing package identity"))?;
            let desired = if needs_override {
                expected.as_ref()
            } else {
                old_version
            };
            let actual_name = alias(desired).map_or(old_name, |(n, _)| n);
            let mut providers = previous.clone();
            if let Some(bindings) = peers.binding_context(side, &old_key, &previous) {
                for (name, binding) in bindings {
                    providers.entry(name).or_insert(binding);
                }
            }
            providers.remove("patch_hash");
            let canonical = peer::canonical_providers(&providers);
            let mut selected = None;
            let mut deferred = None;
            // Patch content changes preserve the locked artifact and edges.
            if needs_patch
                && !needs_override
                && !needs_configuration
                && let Some(patch) = desired_patch(&old_key, metadata, sources.docs)
                && (sources.prefer_baseline || patch.is_none())
            {
                let source = peers.origin_side(&old_key).unwrap_or(side);
                selected = peers
                    .repatch(source, &old_key, patch)
                    .map(|key| crate::reconcile::reference(name, &key));
            }
            // Prefer a proven final-patch graph before deriving a new one.
            for reconstructed in [false, true] {
                for existing_patch in [true, false] {
                    if !existing_patch
                        && selected.is_none()
                        && needs_patch
                        && !needs_override
                        && !needs_configuration
                        && let Some(hash) = desired_patch(&old_key, metadata, sources.docs)
                    {
                        let source = peers.origin_side(&old_key).unwrap_or(side);
                        selected = peers
                            .repatch(source, &old_key, hash)
                            .map(|key| crate::reconcile::reference(name, &key));
                    }
                    for (candidate_side, candidate) in
                        sources.candidates().get(actual_name).into_iter().flatten()
                    {
                        if selected.is_some() {
                            break;
                        }
                        let candidate_ref = crate::reconcile::reference(name, candidate);
                        let matches = if needs_override {
                            accepts(name, desired, &candidate_ref, metadata)
                        } else {
                            graph::package_key(candidate) == graph::package_key(&old_key)
                        };
                        if sources.compatibility[*candidate_side].unchanged(candidate)
                            == reconstructed
                            || !matches
                            || desired_patch(candidate, metadata, sources.docs).is_none()
                            || authorized_patch(candidate, metadata, sources.docs) != existing_patch
                        {
                            continue;
                        }
                        let Some(bindings) =
                            peers.source_context(*candidate_side, candidate, &previous)
                        else {
                            continue;
                        };
                        let affected: BTreeSet<_> = bindings
                            .iter()
                            .filter(|(name, value)| {
                                name.as_str() != "patch_hash"
                                    && canonical.get(*name) != Some(*value)
                            })
                            .map(|(name, _)| name.clone())
                            .collect();
                        let required: BTreeSet<_> = bindings
                            .keys()
                            .filter(|name| {
                                name.as_str() != "patch_hash" && !canonical.contains_key(*name)
                            })
                            .cloned()
                            .collect();
                        if !required.is_empty() {
                            if deferred.is_none() {
                                deferred = Some((
                                    *candidate_side,
                                    *candidate,
                                    required,
                                    affected,
                                    bindings,
                                ));
                            }
                            continue;
                        }
                        if let Some(original) = peers
                            .recontextualize_source(
                                *candidate_side,
                                candidate,
                                &bindings,
                                &previous,
                            )
                            .and_then(|key| {
                                peers.repatch(
                                    *candidate_side,
                                    &key,
                                    desired_patch(candidate, metadata, sources.docs)?,
                                )
                            })
                        {
                            selected = Some(crate::reconcile::reference(name, &original));
                            if !affected.is_empty() {
                                changed_contexts.insert(name.clone(), affected);
                            }
                            break;
                        }
                    }
                    if selected.is_none()
                        && let Some((candidate_side, candidate, required, affected, bindings)) =
                            deferred.take()
                        && let Some(original) = peers
                            .recontextualize_source(candidate_side, candidate, &bindings, &previous)
                            .and_then(|key| {
                                peers.repatch(
                                    candidate_side,
                                    &key,
                                    desired_patch(candidate, metadata, sources.docs)?,
                                )
                            })
                    {
                        selected = Some(crate::reconcile::reference(name, &original));
                        if !affected.is_empty() {
                            changed_contexts.insert(name.clone(), affected);
                        }
                        if !required.is_empty() {
                            added_peers.insert(name.clone(), required);
                        }
                    }
                    if selected.is_some() {
                        break;
                    }
                }
                if selected.is_some() {
                    break;
                }
            }
            if selected.is_none()
                && changed_peers.contains(name)
                && !needs_patch
                && !needs_configuration
            {
                // An explicit enclosing provider may intentionally violate a
                // peer range. Resolve it in the workspace scope before requiring
                // a newly auto-installed provider from the locked candidates.
                unresolved.insert(name.clone());
                changed.insert(name.clone());
                continue;
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
            changed.insert(name.clone());
        }
    }
    if changed.is_empty() && removed.is_empty() {
        let mut peers = PeerChanges::default();
        if !retired.is_empty() {
            peers.supplied.insert(key.into(), retired);
        }
        return Ok(RepairedSnapshot {
            body: result,
            package,
            peers,
            extended: false,
        });
    }
    let mut requirements = PeerRequirements::new();
    let mut affected = PeerRequirements::new();
    for group in ["dependencies", "optionalDependencies"] {
        let entries: Vec<_> = graph::section(&result, group)
            .into_iter()
            .flatten()
            .map(|(n, v)| {
                (
                    n.clone(),
                    v.as_str().expect("selected reference").to_owned(),
                )
            })
            .collect();
        for (name, reference) in entries {
            if reference.starts_with("link:") {
                continue;
            }
            let child = key_in_inputs(&name, &reference, sources.docs, &peers.generated);
            let child_side = peers
                .origin_side(&child)
                .or_else(|| {
                    sources.order.iter().copied().find(|side| {
                        sources.docs[*side]
                            .get("snapshots")
                            .and_then(|v| v.get(&child))
                            .is_some()
                    })
                })
                .ok_or_else(|| missing(&["snapshots", &child], "missing instance"))?;
            let mut names = peer_names(sources.docs[child_side], &child);
            if let Some(bindings) = peers.binding_context(child_side, &child, &previous) {
                names.extend(bindings.into_keys().filter(|name| name != "patch_hash"));
            }
            if !names.is_disjoint(&removed) {
                return Err(missing(
                    &["snapshots", key, &name],
                    "removing a local peer provider requires its enclosing provider context",
                ));
            }
            if names.is_disjoint(&changed) {
                continue;
            }
            affected
                .entry(child)
                .or_default()
                .extend(names.intersection(&changed).cloned());
        }
    }
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
        let reference = ["dependencies", "optionalDependencies"]
            .into_iter()
            .find_map(|group| result.get(group)?.get(&name)?.as_str())
            .expect("selected dependency");
        requirements
            .entry(key_in_inputs(
                &name,
                reference,
                sources.docs,
                &peers.generated,
            ))
            .or_default()
            .extend(names);
    }
    for (name, names) in changed_contexts {
        let reference = ["dependencies", "optionalDependencies"]
            .into_iter()
            .find_map(|group| result.get(group)?.get(&name)?.as_str())
            .expect("selected dependency");
        affected
            .entry(key_in_inputs(
                &name,
                reference,
                sources.docs,
                &peers.generated,
            ))
            .or_default()
            .extend(names);
    }
    let unresolved = if unresolved.is_empty() {
        PeerRequirements::new()
    } else {
        BTreeMap::from([(key.into(), unresolved)])
    };
    Ok(RepairedSnapshot {
        body: result,
        package,
        peers: PeerChanges {
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
        },
        extended: !extended.is_empty(),
    })
}
