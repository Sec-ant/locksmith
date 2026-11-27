//! Select workspace pins, then bind each importer to its final peer providers.
use super::{GraphSources, MergeContext, opaque, peer_names};
use crate::{
    LockfileError, Value, graph, manifest, patch, peer, pin, schema,
    workspace_links::{WorkspaceLinks, injected, same_link_pin},
};
use crate::{catalog::accepts_pin, error::missing, pin::reference};
use serde_json::{Map, json};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

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
    additional: Option<&mut Vec<LockfileError>>,
) -> Result<SelectedImporters, LockfileError> {
    let docs = sources.documents();
    let mut importers = Map::new();
    let selector = PinSelector {
        context,
        output,
        sources,
        workspace_packages: WorkspaceLinks::new(context.manifests, context.configuration),
    };
    let mut configured = ConfiguredRoots::default();
    let overrides = manifest::Overrides::new(output, context.configuration);
    let mut missing_requests = super::MissingRequests::new(additional);
    for (id, package) in context.manifests.get_key_value(".").into_iter().chain(
        context
            .manifests
            .iter()
            .filter(|(id, _)| id.as_str() != "."),
    ) {
        let mut record = importer_metadata(id, package, sources)?;
        let declarations = manifest::declarations(package, &overrides, id)?;
        let mut roots = BTreeMap::new();
        for (name, (group, specifier)) in &declarations {
            let selected = selector.select(
                name,
                group,
                specifier,
                id,
                package,
                preferences.contains(id, name),
            );
            match selected {
                Ok(pin) => {
                    roots.insert((*name).to_owned(), pin);
                }
                Err(error) => missing_requests.record(error)?,
            }
        }
        if missing_requests.first.is_some() {
            continue;
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
        selector.adopt_configured_roots(
            id,
            &mut roots,
            &importers,
            &previous_scopes,
            &mut configured,
            peers,
        );
        let original = roots.clone();
        selector.repatch_roots(id, &mut roots, peers)?;
        selector.stabilize_root_bindings(
            id,
            &mut roots,
            &importers,
            &previous_scopes,
            &original,
            preferences,
            peers,
        )?;
        for (name, (group, specifier)) in declarations {
            if record.get(group).is_none() {
                record[group] = json!({});
            }
            record[group][name] = json!({"specifier":specifier,"version":roots[name].reference});
        }
        importers.insert(id.clone(), record);
    }
    missing_requests.finish()?;
    let records = context
        .manifests
        .keys()
        .filter_map(|id| importers.remove(id).map(|record| (id.clone(), record)))
        .collect();
    Ok(SelectedImporters {
        records,
        configured_sources: configured.sources,
    })
}

#[derive(Clone, Copy)]
enum CandidatePass {
    Exact,
    Compatible,
    Rebuild,
}

#[derive(Clone, Copy)]
struct ImporterDependency<'a> {
    id: &'a str,
    name: &'a str,
    specifier: &'a str,
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
        let docs = sources.documents();
        let configuration = context.configuration;
        let specifier = configuration.registry_specifier(specifier);
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
            let (_, version) = pin::package_key(key).rsplit_once('@')?;
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
        // A configuration edit elsewhere in the workspace need not displace
        // the mainline's direct pin. Assembly still verifies its entire closure.
        if !alternative
            && context.configurations[target].matches_pnpm(context.configuration)
            && !sources.inputs[target].matches_configuration
            && sources.inputs[target].unchanged_rules
            && let Some(pin) = super::preference::dependency(docs[target], id, name)
            && accepts_pin(name, specifier, pin, output, Some(configuration))
        {
            let key = graph::snapshot_key(name, pin, graph::section(docs[target], "snapshots"));
            if sources.inputs[target].compatibility.unchanged(&key)
                && docs[target]["snapshots"].get(&key).is_some()
                && sources.authorized_patch(&key, output)
            {
                return Ok(preferred_local(&key).unwrap_or_else(|| Pin {
                    side: target,
                    key,
                    reference: pin.to_owned(),
                    borrowed: false,
                }));
            }
        }
        let dependency = ImporterDependency {
            id,
            name,
            specifier,
        };
        let selected = self
            .locked_pin(dependency, local, alternative)
            .or_else(|| self.published_pin(name, specifier))
            .ok_or_else(|| {
                super::published::need_version(
                    &["importers", id, group, name],
                    name,
                    specifier,
                    configuration,
                    format!("no locked resolution satisfies {specifier}"),
                )
            })?;
        Ok(preferred_local(&selected.key).unwrap_or(selected))
    }

    fn locked_pin(
        &self,
        dependency: ImporterDependency<'_>,
        local: Option<&String>,
        alternative: bool,
    ) -> Option<Pin> {
        let mut order = self.sources.order;
        let other = self.context.target.other_index();
        if alternative {
            order.sort_by_key(|side| *side != other);
        }
        let mut unpatched = None;
        for pass in [
            CandidatePass::Exact,
            CandidatePass::Compatible,
            CandidatePass::Rebuild,
        ] {
            if self.sources.prefer_baseline && matches!(pass, CandidatePass::Exact) {
                continue;
            }
            for side in order {
                if !self.matches_pass(side, pass) {
                    continue;
                }
                if let Some(pin) = self.importer_pin(dependency, local, side, pass, &mut unpatched)
                {
                    return Some(pin);
                }
                // An alternate branch can witness an artifact through a transitive edge.
                if alternative
                    && side == other
                    && let Some(pin) = self.alternative_pin(dependency, side, pass)
                {
                    return Some(pin);
                }
            }
            if let Some(link) = local {
                return Some(Pin {
                    side: self.context.target.index(),
                    key: link.clone(),
                    reference: link.clone(),
                    borrowed: false,
                });
            }
            if let Some(pin) = self.transitive_pin(dependency, pass, &mut unpatched) {
                return Some(pin);
            }
        }
        unpatched
    }

    fn matches_pass(&self, side: usize, pass: CandidatePass) -> bool {
        self.sources.prefer_baseline
            || self.sources.inputs[side].matches_configuration
                == matches!(pass, CandidatePass::Exact)
    }

    fn importer_pin(
        &self,
        dependency: ImporterDependency<'_>,
        local: Option<&String>,
        side: usize,
        pass: CandidatePass,
        unpatched: &mut Option<Pin>,
    ) -> Option<Pin> {
        let ImporterDependency {
            id,
            name,
            specifier,
        } = dependency;
        let source = &self.sources.inputs[side];
        let importer = source.document.get("importers")?.get(id)?;
        let snapshots = graph::section(source.document, "snapshots");
        for group in schema::DEPENDENCY_GROUPS {
            let Some(dep) = importer.get(group).and_then(|v| v.get(name)) else {
                continue;
            };
            let Some(reference) = dep.get("version").and_then(Value::as_str) else {
                continue;
            };
            if !local.is_some_and(|pin| same_link_pin(pin, reference))
                && !accepts_pin(
                    name,
                    specifier,
                    reference,
                    self.output,
                    Some(self.context.configuration),
                )
                && !(dep.get("specifier").and_then(Value::as_str) == Some(specifier)
                    && opaque(specifier))
            {
                continue;
            }
            let key = graph::snapshot_key(name, reference, snapshots);
            if !source.compatibility.accepts(&key)
                || source.compatibility.unchanged(&key) == matches!(pass, CandidatePass::Rebuild)
            {
                continue;
            }
            if reference.starts_with("link:") {
                return Some(Pin {
                    side,
                    key,
                    reference: reference.to_owned(),
                    borrowed: false,
                });
            }
            if source
                .document
                .get("snapshots")
                .and_then(|records| records.get(&key))
                .is_some()
                && let Ok(desired) = self.sources.desired_patch(&key, self.output)
            {
                let borrowed = !self.sources.prefer_baseline && desired != patch::hash_in_key(&key);
                let pin = Pin {
                    side,
                    key,
                    reference: reference.to_owned(),
                    borrowed,
                };
                if !borrowed {
                    return Some(pin);
                }
                unpatched.get_or_insert(pin);
            }
        }
        None
    }

    fn alternative_pin(
        &self,
        dependency: ImporterDependency<'_>,
        side: usize,
        pass: CandidatePass,
    ) -> Option<Pin> {
        let ImporterDependency {
            id,
            name,
            specifier,
        } = dependency;
        let docs = self.sources.documents();
        let mainline = docs[self.context.target.index()];
        let old = super::preference::dependency(mainline, id, name)?;
        let old_key = graph::snapshot_key(name, old, graph::section(mainline, "snapshots"));
        let locator = pin::package_key(&old_key);
        let (actual, _) = locator.rsplit_once('@')?;
        for &(source, key) in self.sources.candidates().get(actual).into_iter().flatten() {
            if source != side
                || pin::package_key(key) != locator
                || self.sources.inputs[side].compatibility.unchanged(key)
                    == matches!(pass, CandidatePass::Rebuild)
                || !self.sources.authorized_patch(key, self.output)
                || !super::preference::ordinary_edges_match(
                    mainline, &old_key, docs[side], key, true,
                )
            {
                continue;
            }
            let version = reference(name, key);
            if accepts_pin(
                name,
                specifier,
                &version,
                self.output,
                Some(self.context.configuration),
            ) {
                return Some(Pin {
                    side,
                    key: key.to_owned(),
                    reference: version,
                    borrowed: true,
                });
            }
        }
        None
    }

    fn transitive_pin(
        &self,
        dependency: ImporterDependency<'_>,
        pass: CandidatePass,
        unpatched: &mut Option<Pin>,
    ) -> Option<Pin> {
        let ImporterDependency {
            name, specifier, ..
        } = dependency;
        let configuration = self.context.configuration;
        let catalog_specifier = specifier.strip_prefix("catalog:").and_then(|catalog| {
            self.output
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
        let (actual, _) =
            configuration.registry_request(name, catalog_specifier.unwrap_or(specifier));
        for &(side, key) in self.sources.candidates().get(actual).into_iter().flatten() {
            if side == super::PUBLISHED
                || !self.matches_pass(side, pass)
                || self.sources.inputs[side].compatibility.unchanged(key)
                    == matches!(pass, CandidatePass::Rebuild)
            {
                continue;
            }
            let version = reference(name, key);
            if accepts_pin(name, specifier, &version, self.output, Some(configuration))
                && let Ok(desired) = self.sources.desired_patch(key, self.output)
            {
                let pin = Pin {
                    side,
                    key: key.to_owned(),
                    reference: version,
                    borrowed: true,
                };
                if desired == patch::hash_in_key(key) {
                    return Some(pin);
                }
                unpatched.get_or_insert(pin);
            }
        }
        None
    }

    fn published_pin(&self, name: &str, specifier: &str) -> Option<Pin> {
        let registry = self.sources.registry?;
        let configuration = self.context.configuration;
        let effective = specifier
            .strip_prefix("catalog:")
            .and_then(|catalog| {
                configuration
                    .catalogs
                    .get(if catalog.is_empty() {
                        "default"
                    } else {
                        catalog
                    })?
                    .get(name)
            })
            .map_or(specifier, String::as_str);
        let (actual, selector) = configuration.registry_request(name, effective);
        let choice = registry.selected_direct(actual, selector)?;
        self.sources
            .candidates()
            .get(actual)?
            .iter()
            .find_map(|&(side, key)| {
                (side == super::PUBLISHED && choice == pin::package_key(key)).then(|| Pin {
                    side,
                    key: key.to_owned(),
                    reference: reference(name, key),
                    borrowed: true,
                })
            })
    }
}

#[derive(Default)]
struct ConfiguredRoots<'graph> {
    roots: BTreeMap<(usize, String), Vec<(usize, &'graph str)>>,
    sources: BTreeMap<String, usize>,
}

type Providers = BTreeMap<String, String>;

fn workspace_root_providers<'a>(
    id: &str,
    importers: &'a Map<String, Value>,
    context: &MergeContext<'_>,
) -> impl Iterator<Item = (String, String)> + use<'a> {
    let root = if id != "." && context.configuration.resolution.workspace_root_peers {
        importers.get(".")
    } else {
        None
    };
    schema::DEPENDENCY_GROUPS
        .into_iter()
        .flat_map(move |group| {
            root.and_then(|root| graph::section(root, group))
                .into_iter()
                .flatten()
        })
        .filter_map(|(name, dep)| {
            let reference = dep.get("version")?.as_str()?;
            Some((name.clone(), peer::root_provider(".", reference)))
        })
}

fn importer_metadata(
    id: &str,
    package: &Value,
    sources: &GraphSources<'_>,
) -> Result<Value, LockfileError> {
    let docs = sources.documents();
    let mut record = sources
        .order
        .iter()
        .find_map(|side| docs[*side].get("importers")?.get(id))
        .cloned()
        .unwrap_or_else(|| json!({}));
    for group in schema::DEPENDENCY_GROUPS {
        record
            .as_object_mut()
            .ok_or_else(|| missing(&["importers", id], "expected importer mapping"))?
            .remove(group);
    }
    for (field, source) in [
        ("dependenciesMeta", package.get("dependenciesMeta")),
        (
            "linkDirectory",
            manifest::link_directory(package, sources.configuration),
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
    Ok(record)
}

impl<'graph> PinSelector<'_, 'graph> {
    fn adopt_configured_roots(
        &self,
        id: &str,
        roots: &mut BTreeMap<String, Pin>,
        importers: &Map<String, Value>,
        previous_scopes: &[Providers; 4],
        configured: &mut ConfiguredRoots<'graph>,
        peers: &mut peer::Merger<'_>,
    ) {
        let has_witness = roots.values().any(|pin| {
            !pin.reference.starts_with("link:")
                && !self.sources.inputs[pin.side].unchanged_rules
                && !configured
                    .roots
                    .entry((pin.side, pin.key.clone()))
                    .or_insert_with(|| {
                        configured_peer_roots(&pin.key, pin.side, self.output, self.sources)
                    })
                    .is_empty()
        });
        if has_witness {
            let mut available = roots
                .iter()
                .map(|(name, pin)| (name.clone(), peer::root_provider(id, &pin.reference)))
                .collect::<BTreeMap<_, _>>();
            for (name, reference) in workspace_root_providers(id, importers, self.context) {
                available.entry(name).or_insert(reference);
            }
            let available = peer::canonical_providers(&available);
            for (name, pin) in roots {
                if pin.reference.starts_with("link:")
                    || self.sources.inputs[pin.side].unchanged_rules
                {
                    continue;
                }
                let witnesses = configured
                    .roots
                    .entry((pin.side, pin.key.clone()))
                    .or_insert_with(|| {
                        configured_peer_roots(&pin.key, pin.side, self.output, self.sources)
                    });
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
                    configured.sources.insert((*candidate).to_owned(), *source);
                    pin.side = *source;
                    pin.reference = reference(name, candidate);
                    pin.key = (*candidate).to_owned();
                    pin.borrowed = true;
                    break;
                }
            }
        }
    }

    fn repatch_roots(
        &self,
        id: &str,
        roots: &mut BTreeMap<String, Pin>,
        peers: &mut peer::Merger<'_>,
    ) -> Result<(), LockfileError> {
        let Self {
            sources, output, ..
        } = self;
        for (name, pin) in roots {
            if pin.reference.starts_with("link:") || sources.authorized_patch(&pin.key, output) {
                continue;
            }
            let hash = sources
                .desired_patch(&pin.key, output)
                .expect("selected patch");
            pin.key = peers.repatch(pin.side, &pin.key, hash).ok_or_else(|| {
                missing(
                    &["importers", id, name],
                    "patch identity requires the original peer bindings",
                )
            })?;
            pin.reference = reference(name, &pin.key);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn stabilize_root_bindings(
        &self,
        id: &str,
        roots: &mut BTreeMap<String, Pin>,
        importers: &Map<String, Value>,
        previous_scopes: &[Providers; 4],
        original: &BTreeMap<String, Pin>,
        preferences: &super::preference::Preferences,
        peers: &mut peer::Merger<'_>,
    ) -> Result<(), LockfileError> {
        let Self {
            sources, output, ..
        } = self;
        let docs = sources.documents();
        let mut used_sources = [false; 4];
        for pin in original.values().filter(|pin| !pin.borrowed) {
            used_sources[pin.side] = true;
        }
        let previous_bindings = previous_scopes.each_ref().map(peer::canonical_providers);
        // Provider changes propagate at most one root dependency per pass in an
        // acyclic peer graph. Existing coherent cycles settle without changes.
        let mut unsettled = None;
        for _ in 0..=roots.len() {
            let mut providers = Providers::new();
            for (name, reference) in workspace_root_providers(id, importers, self.context) {
                providers.insert(name, reference);
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
            for (name, pin) in original {
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
                let existing = self
                    .importer_witness(
                        id,
                        name,
                        pin,
                        &names,
                        &previous_bindings,
                        &provider_bindings,
                    )
                    .or_else(|| {
                        if preferences.contains(id, name) {
                            self.alternative_witness(pin, &provider_bindings, peers)
                        } else {
                            None
                        }
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
                let hash = sources
                    .desired_patch(&rebound, output)
                    .expect("selected patch");
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
        Ok(())
    }

    fn importer_witness(
        &self,
        id: &str,
        name: &str,
        pin: &Pin,
        names: &BTreeSet<String>,
        previous_bindings: &[Cow<'_, Providers>; 4],
        provider_bindings: &Providers,
    ) -> Option<(usize, String)> {
        let docs = self.sources.documents();
        // Reuse a locked instance of the same artifact when an importer
        // actually used it under the selected providers. An opaque peer
        // suffix also has this witness; reconstruct only mixed contexts.
        self.sources.order.iter().find_map(|&other| {
            let reference = super::preference::dependency(docs[other], id, name)?;
            let key =
                graph::snapshot_key(name, reference, graph::section(docs[other], "snapshots"));
            if pin::package_key(&key) != pin::package_key(&pin.key)
                || patch::hash_in_key(&key) != patch::hash_in_key(&pin.key)
                || !self.sources.inputs[other].compatibility.accepts(&key)
                || docs[other]["packages"][pin::package_key(&key)]
                    != docs[pin.side]["packages"][pin::package_key(&pin.key)]
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
                .chain(names)
                .all(|peer| previous_bindings[other].get(peer) == provider_bindings.get(peer));
            same_scope.then_some((other, key))
        })
    }

    fn alternative_witness(
        &self,
        pin: &Pin,
        provider_bindings: &Providers,
        peers: &mut peer::Merger<'_>,
    ) -> Option<(usize, String)> {
        let docs = self.sources.documents();
        let locator = pin::package_key(&pin.key);
        let (actual, _) = locator.rsplit_once('@')?;
        self.sources
            .candidates()
            .get(actual)?
            .iter()
            .find_map(|&(side, key)| {
                (key != pin.key
                    && pin::package_key(key) == locator
                    && patch::hash_in_key(key) == patch::hash_in_key(&pin.key)
                    && docs[side]["packages"][locator] == docs[pin.side]["packages"][locator]
                    && peers.witnesses_scope(side, (pin.side, &pin.key), key, provider_bindings)
                    && super::preference::ordinary_edges_match(
                        docs[pin.side],
                        &pin.key,
                        docs[side],
                        key,
                        true,
                    ))
                .then(|| (side, key.to_owned()))
            })
    }
}

fn configured_peer_roots<'a>(
    key: &str,
    side: usize,
    metadata: &Value,
    sources: &GraphSources<'a>,
) -> Vec<(usize, &'a str)> {
    let package_key = pin::package_key(key);
    let Some((parent, version)) = package_key.rsplit_once('@') else {
        return Vec::new();
    };
    if version.parse::<node_semver::Version>().is_err() {
        return Vec::new();
    }
    let body = &sources.inputs[side].document["snapshots"][key];
    let package = &sources.inputs[side].document["packages"][package_key];
    let names = peer_names(sources.inputs[side].document, key);
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
            let before = sources.inputs[side].rules.relevant(name, parent, version);
            let after = sources.final_rules.relevant(name, parent, version);
            let changed = before != after
                || (after.len() > 1
                    && sources.inputs[side].tie_break != sources.override_tie_break);
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
                || pin::package_key(candidate) != package_key
                || sources.inputs[*source].document["packages"][package_key] != *package
                || !sources.authorized_patch(candidate, metadata)
                || peer_names(sources.inputs[*source].document, candidate) == names
            {
                return false;
            }
            let replacement = &sources.inputs[*source].document["snapshots"][*candidate];
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
                        && sources.inputs[*source].tie_break != sources.override_tie_break)
                        || sources.inputs[*source]
                            .rules
                            .relevant(name, parent, version)
                            != *after)
                {
                    return false;
                }
                if pin::package_key(replacement)
                    != pin::package_key(reference.as_str().expect("dependency reference"))
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
