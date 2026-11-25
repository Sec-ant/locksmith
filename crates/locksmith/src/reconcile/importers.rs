//! Select workspace pins, then bind each importer to its final peer providers.
use super::{
    GraphSources, MergeContext, accepts, authorized_patch, desired_patch, missing, opaque,
    peer_names, reference,
};
use crate::{
    LockfileError, Value, graph, manifest, peer,
    workspace_links::{WorkspaceLinks, injected},
};
use serde_json::{Map, json};
use std::collections::BTreeMap;

pub(super) struct SelectedImporters {
    pub(super) records: Map<String, Value>,
    pub(super) configured_sources: BTreeMap<String, usize>,
}

#[derive(Clone)]
struct Pin {
    side: usize,
    reference: String,
    key: String,
    // A pin borrowed from another context must check every visible provider.
    borrowed: bool,
}

pub(super) fn select(
    context: &MergeContext<'_>,
    output: &Value,
    sources: &GraphSources<'_>,
    peers: &mut peer::Merger<'_>,
    preferences: &super::preference::Preferences,
) -> Result<SelectedImporters, LockfileError> {
    let docs = sources.docs;
    let mut importers = Map::new();
    let selector = PinSelector {
        context,
        output,
        sources,
        workspace_packages: WorkspaceLinks::new(context.manifests, context.configuration),
    };
    let mut configured_roots = BTreeMap::new();
    let mut configured_sources = BTreeMap::new();
    let overrides = manifest::Overrides::new(output, context.configuration);
    for (id, package) in context.manifests {
        let mut record = sources
            .order
            .iter()
            .find_map(|side| docs[*side].get("importers")?.get(id))
            .cloned()
            .unwrap_or_else(|| json!({}));
        for group in manifest::GROUPS {
            record
                .as_object_mut()
                .ok_or_else(|| missing(&["importers", id], "expected importer mapping"))?
                .remove(group);
        }
        for (field, source) in [
            ("dependenciesMeta", package.get("dependenciesMeta")),
            (
                "linkDirectory",
                manifest::link_directory(package, context.configuration),
            ),
            (
                "publishDirectory",
                package
                    .get("publishConfig")
                    .and_then(|v| v.get("directory")),
            ),
        ] {
            if let Some(value) = source {
                record[field] = value.clone();
            } else {
                record.as_object_mut().unwrap().remove(field);
            }
        }
        let declarations = manifest::declarations(package, &overrides, id)?;
        let mut roots = BTreeMap::new();
        for (name, (group, specifier)) in &declarations {
            let pin = selector.select(
                name,
                group,
                specifier,
                id,
                package,
                preferences.contains(id, name),
            )?;
            roots.insert((*name).to_owned(), pin);
        }
        let previous_scopes: [BTreeMap<String, String>; 4] = std::array::from_fn(|side| {
            let global = context
                .configurations
                .get(side)
                .copied()
                .unwrap_or(context.configuration)
                .resolution
                .workspace_root_peers;
            peer::importer_providers(&docs[side]["importers"], id, global)
        });
        let has_witness = roots.values().any(|pin| {
            !pin.reference.starts_with("link:")
                && !sources.unchanged_rules[pin.side]
                && !configured_roots
                    .entry((pin.side, pin.key.clone()))
                    .or_insert_with(|| configured_peer_roots(&pin.key, pin.side, output, sources))
                    .is_empty()
        });
        if has_witness {
            let mut available = roots
                .iter()
                .map(|(name, pin)| (name.clone(), peer::root_provider(id, &pin.reference)))
                .collect::<BTreeMap<_, _>>();
            if id != "." && context.configuration.resolution.workspace_root_peers {
                for group in manifest::GROUPS {
                    for (name, dep) in importers
                        .get(".")
                        .and_then(|root| graph::section(root, group))
                        .into_iter()
                        .flatten()
                    {
                        if let Some(reference) = dep.get("version").and_then(Value::as_str) {
                            available
                                .entry(name.clone())
                                .or_insert_with(|| peer::root_provider(".", reference));
                        }
                    }
                }
            }
            let available = peer::canonical_providers(&available);
            for (name, pin) in &mut roots {
                if pin.reference.starts_with("link:") || sources.unchanged_rules[pin.side] {
                    continue;
                }
                let witnesses = configured_roots
                    .entry((pin.side, pin.key.clone()))
                    .or_insert_with(|| configured_peer_roots(&pin.key, pin.side, output, sources));
                if witnesses.is_empty() {
                    continue;
                }
                let Some(before) =
                    peers.binding_context(pin.side, &pin.key, &previous_scopes[pin.side])
                else {
                    continue;
                };
                for (source, candidate) in witnesses {
                    let Some(after) =
                        peers.binding_context(*source, candidate, &previous_scopes[*source])
                    else {
                        continue;
                    };
                    // Preserve bindings outside the final workspace scope. Available
                    // providers, including newly required peers, are rebound below.
                    if before.iter().any(|(name, binding)| {
                        after
                            .get(name)
                            .is_none_or(|next| next != binding && !available.contains_key(name))
                    }) || after
                        .keys()
                        .any(|name| !before.contains_key(name) && !available.contains_key(name))
                    {
                        continue;
                    }
                    configured_sources.insert((*candidate).to_owned(), *source);
                    pin.side = *source;
                    pin.reference = reference(name, candidate);
                    pin.key = (*candidate).to_owned();
                    pin.borrowed = true;
                    break;
                }
            }
        }
        let original = roots.clone();
        for (name, pin) in &mut roots {
            if pin.reference.starts_with("link:")
                || authorized_patch(&pin.key, output, sources.docs)
            {
                continue;
            }
            let hash = desired_patch(&pin.key, output, sources.docs).expect("selected patch");
            pin.key = peers.repatch(pin.side, &pin.key, hash).ok_or_else(|| {
                missing(
                    &["importers", id, name],
                    "patch identity requires the original peer bindings",
                )
            })?;
            pin.reference = reference(name, &pin.key);
        }
        let mut used_sources = [false; 4];
        for pin in original.values().filter(|pin| !pin.borrowed) {
            used_sources[pin.side] = true;
        }
        let previous_bindings = previous_scopes.each_ref().map(peer::canonical_providers);
        // Provider changes propagate at most one root dependency per pass in an
        // acyclic peer graph. Existing coherent cycles settle without changes.
        let mut unsettled = None;
        for _ in 0..=roots.len() {
            let mut providers: BTreeMap<String, String> = BTreeMap::new();
            if id != "."
                && context.configuration.resolution.workspace_root_peers
                && let Some(root) = importers.get(".")
            {
                for group in manifest::GROUPS {
                    for (name, dep) in graph::section(root, group).into_iter().flatten() {
                        if let Some(version) = dep.get("version").and_then(Value::as_str) {
                            providers.insert(name.clone(), peer::root_provider(".", version));
                        }
                    }
                }
            }
            providers.extend(
                roots
                    .iter()
                    .map(|(name, pin)| (name.clone(), peer::root_provider(id, &pin.reference))),
            );
            let provider_bindings = peer::canonical_providers(&providers);
            // Roots selected from the same input see the same provider changes during this pass.
            let changed_providers: [BTreeMap<_, _>; 4] = std::array::from_fn(|side| {
                if !used_sources[side] {
                    return BTreeMap::new();
                }
                let previous = &previous_scopes[side];
                providers
                    .iter()
                    .filter(|(peer, value)| previous.get(*peer) != Some(*value))
                    .map(|(peer, value)| (peer.clone(), value.clone()))
                    .collect::<BTreeMap<_, _>>()
            });
            unsettled = None;
            let mut unresolved = None;
            peers.begin_pass();
            for (name, pin) in &original {
                if pin.reference.starts_with("link:") {
                    continue;
                }
                let names = peer_names(docs[pin.side], &pin.key);
                let changes = if pin.borrowed {
                    &providers
                } else {
                    &changed_providers[pin.side]
                };
                let needs_rebinding = if pin.borrowed {
                    match peers.source_context(pin.side, &pin.key, &previous_scopes[pin.side]) {
                        Some(bindings) => bindings.iter().any(|(peer, old)| {
                            peer != "patch_hash"
                                && provider_bindings.get(peer).is_some_and(|new| new != old)
                        }),
                        None => names.iter().any(|peer| changes.contains_key(peer)),
                    }
                } else {
                    names.iter().any(|peer| changes.contains_key(peer))
                };
                if !needs_rebinding {
                    continue;
                }
                // Reuse a locked instance of the same artifact when an importer
                // actually used it under the selected providers. An opaque peer
                // suffix also has this witness; reconstruct only mixed contexts.
                let existing = sources.order.iter().find_map(|&other| {
                    let record = docs[other].get("importers")?.get(id)?;
                    let reference = manifest::GROUPS.iter().find_map(|group| {
                        record.get(*group)?.get(name)?.get("version")?.as_str()
                    })?;
                    let key = graph::snapshot_key(
                        name,
                        reference,
                        graph::section(docs[other], "snapshots"),
                    );
                    if graph::package_key(&key) != graph::package_key(&pin.key)
                        || peer::patch(&key) != peer::patch(&pin.key)
                        || !sources.compatibility[other].accepts(&key)
                        || docs[other]["packages"][graph::package_key(&key)]
                            != docs[pin.side]["packages"][graph::package_key(&pin.key)]
                        || docs[other].get("snapshots")?.get(&key).is_none()
                    {
                        return None;
                    }
                    if key != pin.key
                        && !super::preference::ordinary_edges_match(
                            docs[pin.side],
                            &pin.key,
                            docs[other],
                            &key,
                            true,
                        )
                    {
                        return None;
                    }
                    let same_scope = peer_names(docs[other], &key)
                        .iter()
                        .chain(names.iter())
                        .all(|peer| {
                            previous_bindings[other].get(peer) == provider_bindings.get(peer)
                        });
                    same_scope.then_some((other, key))
                });
                let existing = existing.or_else(|| {
                    if !preferences.contains(id, name) {
                        return None;
                    }
                    let locator = graph::package_key(&pin.key);
                    let (actual, _) = locator.rsplit_once('@')?;
                    sources
                        .candidates()
                        .get(actual)?
                        .iter()
                        .find_map(|&(side, key)| {
                            (key != pin.key
                                && graph::package_key(key) == locator
                                && peer::patch(key) == peer::patch(&pin.key)
                                && docs[side]["packages"][locator]
                                    == docs[pin.side]["packages"][locator]
                                && peers.witnesses_scope(
                                    side,
                                    (pin.side, &pin.key),
                                    key,
                                    &provider_bindings,
                                )
                                && super::preference::ordinary_edges_match(
                                    docs[pin.side],
                                    &pin.key,
                                    docs[side],
                                    key,
                                    true,
                                ))
                            .then(|| (side, key.to_owned()))
                        })
                });
                let (resolved_side, rebound) = if let Some(existing) = existing {
                    existing
                } else {
                    let Some(key) = peers.recontextualize_source(
                        pin.side,
                        &pin.key,
                        changes,
                        &previous_scopes[pin.side],
                    ) else {
                        unresolved.get_or_insert(name);
                        continue;
                    };
                    (pin.side, key)
                };
                let hash = desired_patch(&rebound, output, sources.docs).expect("selected patch");
                let rebound = peers
                    .repatch(resolved_side, &rebound, hash)
                    .ok_or_else(|| {
                        missing(
                            &["importers", id, name],
                            "patch identity requires the original peer bindings",
                        )
                    })?;
                let version = reference(name, &rebound);
                let entry = roots.get_mut(name).unwrap();
                if entry.reference != version {
                    unsettled = Some(name);
                }
                entry.side = resolved_side;
                entry.reference = version;
                entry.key = rebound;
            }
            peers.finish_pass(unsettled.is_none() && unresolved.is_none());
            if unsettled.is_none() {
                if let Some(name) = unresolved {
                    return Err(missing(
                        &["importers", id, name],
                        "peer bindings cannot be rebuilt from the supplied graphs",
                    ));
                }
                break;
            }
        }
        if let Some(name) = unsettled {
            return Err(missing(
                &["importers", id, name],
                "peer identities do not reach a stable context",
            ));
        }
        for (name, (group, specifier)) in declarations {
            if record.get(group).is_none() {
                record[group] = json!({});
            }
            record[group][name] = json!({"specifier":specifier,"version":roots[name].reference});
        }
        importers.insert(id.clone(), record);
    }
    Ok(SelectedImporters {
        records: importers,
        configured_sources,
    })
}

enum CandidatePass {
    Exact,
    Compatible,
    Rebuild,
}

struct PinSelector<'a, 'graph> {
    context: &'a MergeContext<'a>,
    output: &'a Value,
    sources: &'a GraphSources<'graph>,
    workspace_packages: WorkspaceLinks<'a>,
}

impl PinSelector<'_, '_> {
    fn select(
        &self,
        name: &str,
        group: &str,
        specifier: &str,
        id: &str,
        package: &Value,
        alternative: bool,
    ) -> Result<Pin, LockfileError> {
        let Self {
            context,
            sources,
            output,
            workspace_packages,
        } = self;
        let docs = sources.docs;
        let resolved = workspace_packages.resolve(name, specifier, id, package)?;
        let local = resolved.as_ref().map(|(link, _)| link);
        if specifier.starts_with("workspace:") && !injected(name, package, context.configuration) {
            let link = local.ok_or_else(|| {
                missing(
                    &["importers", id, group, name],
                    format!("no locked resolution satisfies {specifier}"),
                )
            })?;
            return Ok(Pin {
                side: context.target.index(),
                key: link.clone(),
                reference: link.clone(),
                borrowed: false,
            });
        }
        let target = context.target.index();
        let preferred_local = |key: &str| -> Option<Pin> {
            let (link, package) = resolved.as_ref()?;
            let workspace_version = package.as_ref()?["version"]
                .as_str()?
                .parse::<node_semver::Version>()
                .ok()?;
            let (_, version) = graph::package_key(key).rsplit_once('@')?;
            let locked = version.parse::<node_semver::Version>().ok()?;
            let prefer = context
                .configuration
                .resolution
                .parameters
                .get("preferWorkspacePackages")
                .is_some_and(|v| v == true);
            (workspace_version >= locked || prefer).then(|| Pin {
                side: target,
                key: link.clone(),
                reference: link.clone(),
                borrowed: false,
            })
        };
        let mut order = sources.order;
        // A configuration edit elsewhere in the workspace need not displace
        // the mainline's direct pin. Assembly still verifies its entire closure.
        if !alternative
            && context.configurations[target].matches_pnpm(context.configuration)
            && !sources.unchanged_configuration[target]
            && sources.unchanged_rules[target]
            && let Some(pin) = super::preference::dependency(docs[target], id, name)
            && accepts(name, specifier, pin, output)
        {
            let key = graph::snapshot_key(name, pin, graph::section(docs[target], "snapshots"));
            if sources.compatibility[target].unchanged(&key)
                && docs[target]["snapshots"].get(&key).is_some()
                && authorized_patch(&key, output, sources.docs)
            {
                return Ok(preferred_local(&key).unwrap_or_else(|| Pin {
                    side: target,
                    key,
                    reference: pin.to_owned(),
                    borrowed: false,
                }));
            }
        }
        if alternative {
            let other = context.target.other_index();
            order.sort_by_key(|side| *side != other);
        }
        let mut borrowed = false;
        let mut selected = None;
        let mut unpatched_selection = None;
        for pass in [
            CandidatePass::Exact,
            CandidatePass::Compatible,
            CandidatePass::Rebuild,
        ] {
            if sources.prefer_baseline && matches!(pass, CandidatePass::Exact) {
                continue;
            }
            for side in order {
                if !sources.prefer_baseline
                    && sources.unchanged_configuration[side] != matches!(pass, CandidatePass::Exact)
                {
                    continue;
                }
                for source_group in manifest::GROUPS {
                    let Some(dep) = docs[side]
                        .get("importers")
                        .and_then(|v| v.get(id))
                        .and_then(|v| v.get(source_group))
                        .and_then(|v| v.get(name))
                    else {
                        continue;
                    };
                    let Some(reference) = dep.get("version").and_then(Value::as_str) else {
                        continue;
                    };
                    if local.map(String::as_str) == Some(reference)
                        || accepts(name, specifier, reference, output)
                        || (dep.get("specifier").and_then(Value::as_str) == Some(specifier)
                            && opaque(specifier))
                    {
                        let key = graph::snapshot_key(
                            name,
                            reference,
                            graph::section(docs[side], "snapshots"),
                        );
                        if !sources.compatibility[side].accepts(&key)
                            || sources.compatibility[side].unchanged(&key)
                                == matches!(pass, CandidatePass::Rebuild)
                        {
                            continue;
                        }
                        if reference.starts_with("link:") {
                            selected = Some((side, reference.to_owned(), key));
                            break;
                        }
                        if docs[side]
                            .get("snapshots")
                            .and_then(|records| records.get(&key))
                            .is_some()
                            && desired_patch(&key, output, sources.docs).is_some()
                        {
                            if sources.prefer_baseline
                                || authorized_patch(&key, output, sources.docs)
                            {
                                selected = Some((side, reference.to_owned(), key));
                                break;
                            }
                            unpatched_selection.get_or_insert((side, reference.to_owned(), key));
                        }
                    }
                }
                // A newly added direct dependency may exist only transitively on
                // the other branch. During a local retry, that branch can still
                // witness the selected artifact under the final configuration.
                if selected.is_none()
                    && alternative
                    && side == context.target.other_index()
                    && let Some(old) =
                        super::preference::dependency(docs[context.target.index()], id, name)
                {
                    let old_key = graph::snapshot_key(
                        name,
                        old,
                        graph::section(docs[context.target.index()], "snapshots"),
                    );
                    let locator = graph::package_key(&old_key);
                    if let Some((actual, _)) = locator.rsplit_once('@') {
                        for &(source, key) in sources.candidates().get(actual).into_iter().flatten()
                        {
                            if source != side
                                || graph::package_key(key) != locator
                                || sources.compatibility[side].unchanged(key)
                                    == matches!(pass, CandidatePass::Rebuild)
                                || !authorized_patch(key, output, sources.docs)
                                || !super::preference::ordinary_edges_match(
                                    docs[context.target.index()],
                                    &old_key,
                                    docs[side],
                                    key,
                                    true,
                                )
                            {
                                continue;
                            }
                            let version = reference(name, key);
                            if accepts(name, specifier, &version, output) {
                                selected = Some((side, version, key.to_owned()));
                                borrowed = true;
                                break;
                            }
                        }
                    }
                }
                if selected.is_some() {
                    break;
                }
            }
            if selected.is_none()
                && let Some(link) = local
            {
                selected = Some((context.target.index(), link.clone(), link.clone()));
            }
            if selected.is_none() {
                let catalog_specifier = specifier.strip_prefix("catalog:").and_then(|catalog| {
                    output
                        .get("catalogs")?
                        .get(if catalog.is_empty() {
                            "default"
                        } else {
                            catalog
                        })?
                        .get(name)?
                        .get("specifier")?
                        .as_str()
                });
                let actual_name = manifest::alias(catalog_specifier.unwrap_or(specifier))
                    .map_or(name, |(name, _)| name);
                for (side, key) in sources.candidates().get(actual_name).into_iter().flatten() {
                    if *side == super::PUBLISHED
                        || (!sources.prefer_baseline
                            && sources.unchanged_configuration[*side]
                                != matches!(pass, CandidatePass::Exact))
                        || sources.compatibility[*side].unchanged(key)
                            == matches!(pass, CandidatePass::Rebuild)
                    {
                        continue;
                    }
                    let reference = reference(name, key);
                    if accepts(name, specifier, &reference, output)
                        && desired_patch(key, output, sources.docs).is_some()
                    {
                        if authorized_patch(key, output, sources.docs) {
                            selected = Some((*side, reference, (*key).into()));
                            borrowed = true;
                            break;
                        }
                        unpatched_selection.get_or_insert((*side, reference, (*key).into()));
                    }
                }
            }
            if selected.is_some() {
                break;
            }
        }
        if selected.is_none() {
            selected = unpatched_selection;
            if selected.is_some() {
                borrowed = true;
            }
        }
        if selected.is_none()
            && let Some(registry) = sources.registry
        {
            let effective = specifier
                .strip_prefix("catalog:")
                .and_then(|c| {
                    context
                        .configuration
                        .catalogs
                        .get(if c.is_empty() { "default" } else { c })?
                        .get(name)
                })
                .map(String::as_str)
                .unwrap_or(specifier);
            let (actual, selector) = manifest::alias(effective).unwrap_or((name, effective));
            if let Some(choice) = registry.selected(actual, selector) {
                selected = sources
                    .candidates()
                    .get(actual)
                    .into_iter()
                    .flatten()
                    .find_map(|&(side, key)| {
                        (side == super::PUBLISHED && choice == graph::package_key(key))
                            .then(|| (side, reference(name, key), key.to_owned()))
                    });
            }
            borrowed = selected.is_some();
        }
        let (side, version, key) = selected.ok_or_else(|| {
            super::published::need_version(
                &["importers", id, group, name],
                name,
                specifier,
                context.configuration,
                format!("no locked resolution satisfies {specifier}"),
            )
        })?;
        Ok(preferred_local(&key).unwrap_or(Pin {
            side,
            reference: version,
            key,
            borrowed,
        }))
    }
}

fn configured_peer_roots<'a>(
    key: &str,
    side: usize,
    metadata: &Value,
    sources: &GraphSources<'a>,
) -> Vec<(usize, &'a str)> {
    let package_key = graph::package_key(key);
    let Some((parent, version)) = package_key.rsplit_once('@') else {
        return Vec::new();
    };
    if version.parse::<node_semver::Version>().is_err() {
        return Vec::new();
    }
    let body = &sources.docs[side]["snapshots"][key];
    let package = &sources.docs[side]["packages"][package_key];
    let names = peer_names(sources.docs[side], key);
    let mut dependencies = Vec::new();
    for group in ["dependencies", "optionalDependencies"] {
        for (name, reference) in graph::section(body, group).into_iter().flatten() {
            if package
                .get("peerDependencies")
                .and_then(|v| v.get(name))
                .is_some()
            {
                continue;
            }
            let before = sources.rules[side].relevant(name, parent, version);
            let after = sources.final_rules.relevant(name, parent, version);
            let changed = before != after
                || (after.len() > 1
                    && sources.source_tie_breaks[side] != sources.override_tie_break);
            dependencies.push((group, name, reference, after, changed));
        }
    }
    if !dependencies.iter().any(|(_, _, _, _, changed)| *changed) {
        return Vec::new();
    }
    sources
        .candidates()
        .get(parent)
        .into_iter()
        .flatten()
        .copied()
        .filter(|(source, candidate)| {
            if *candidate == key
                || graph::package_key(candidate) != package_key
                || sources.docs[*source]["packages"][package_key] != *package
                || !authorized_patch(candidate, metadata, sources.docs)
                || peer_names(sources.docs[*source], candidate) == names
            {
                return false;
            }
            let replacement = &sources.docs[*source]["snapshots"][*candidate];
            for group in ["dependencies", "optionalDependencies"] {
                let before = graph::section(body, group);
                let after = graph::section(replacement, group);
                if before.map(Map::len) != after.map(Map::len)
                    || before
                        .into_iter()
                        .flatten()
                        .any(|(name, _)| after.is_none_or(|m| !m.contains_key(name)))
                {
                    return false;
                }
            }
            let other_fields = |field: &str| {
                !matches!(
                    field,
                    "dependencies" | "optionalDependencies" | "transitivePeerDependencies"
                )
            };
            if !body
                .as_object()
                .unwrap()
                .iter()
                .filter(|(field, _)| other_fields(field))
                .eq(replacement
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(field, _)| other_fields(field)))
            {
                return false;
            }
            let mut changed_resolution = false;
            for (group, name, reference, after, changed) in &dependencies {
                let Some(replacement) = replacement
                    .get(*group)
                    .and_then(|v| v.get(*name))
                    .and_then(Value::as_str)
                else {
                    return false;
                };
                if *changed
                    && ((after.len() > 1
                        && sources.source_tie_breaks[*source] != sources.override_tie_break)
                        || sources.rules[*source].relevant(name, parent, version) != *after)
                {
                    return false;
                }
                if graph::package_key(replacement)
                    != graph::package_key(reference.as_str().expect("dependency reference"))
                {
                    if !changed {
                        return false;
                    }
                    changed_resolution = true;
                }
            }
            changed_resolution
        })
        .collect()
}
