//! Add published releases to the set of graph sources under final workspace policy.
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
};

use indexmap::IndexMap;
use serde_json::{Map, json};

use super::MergeContext;
use crate::{
    LockfileError, RegistryMetadata, RegistryRequest, Value, WorkspaceConfig, graph, manifest,
    patch, pin, schema,
};
use crate::{catalog::accepts_pin, error::missing, patch::desired_hash, pin::reference};

// These pnpm releases started recording a declared peer even when only its
// ordinary dependency supplied it in the resolved graph (pnpm issue #17056).
fn records_fallback_peers(config: &WorkspaceConfig) -> bool {
    use node_semver::Version;
    config
        .pnpm_version
        .as_ref()
        .is_none_or(|v| match v.major() {
            11 => v >= &Version::from((11, 28, 7)),
            12 => v >= &Version::from((12, 15, 0)),
            major => major > 12,
        })
}

/// The edges and peers pnpm resolves from a published manifest once the
/// workspace overrides apply.
#[derive(Default)]
struct PublishedDeclarations {
    /// Group and selector by alias, in manifest order.
    ordinary: IndexMap<String, (&'static str, String)>,
    /// Range and optionality by peer name.
    peers: BTreeMap<String, (String, bool)>,
}

fn published_declarations(
    raw: &Value,
    key: &str,
    rules: &manifest::Overrides<'_>,
    config: &WorkspaceConfig,
    needed: &mut BTreeSet<String>,
) -> Result<PublishedDeclarations, LockfileError> {
    let bundle = raw
        .get("bundledDependencies")
        .or_else(|| raw.get("bundleDependencies"));
    // pnpm 9.15.1 removed a package's own name from its peer requirements.
    let exclude_self = config
        .pnpm_version
        .as_ref()
        .is_none_or(|v| v >= &node_semver::Version::from((9, 15, 1)));
    // pnpm 12 before 12.17 used the promoted selector for an existing optional edge.
    let promotes_over_optional = config
        .pnpm_version
        .as_ref()
        .is_some_and(|v| v.major() == 12 && v < &node_semver::Version::from((12, 17, 0)));
    let own_name = raw["name"].as_str();
    let ignores_optional = rules.optional_exclusion()?;
    let mut declarations = PublishedDeclarations::default();
    let mut removed_dependencies = BTreeSet::new();
    let mut removed_optional = BTreeSet::new();
    let mut effective_optional = BTreeSet::new();
    for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
        let Some(entries) = raw.get(group) else {
            continue;
        };
        for (name, value) in entries
            .as_object()
            .ok_or_else(|| missing(&[key, group], "expected a dependency mapping"))?
        {
            let value = value
                .as_str()
                .ok_or_else(|| missing(&[key, group, name], "expected a dependency selector"))?;
            let replacement = rules.select(raw, name, value)?;
            let spec = match replacement {
                Some(value) => rules.resolve(".", name, value)?,
                None => Some(Cow::Borrowed(value)),
            };
            let Some(mut spec) = spec else {
                if group == "dependencies" {
                    removed_dependencies.insert(name.as_str());
                }
                continue;
            };
            let promoted = group == "peerDependencies"
                && replacement.is_some()
                && !manifest::valid_peer_range(&spec);
            // pnpm drops a same-name ordinary edge when ignoring an optional
            // dependency. The peer's original range then applies again.
            let restore_peer = promoted && removed_optional.contains(name.as_str());
            if restore_peer {
                spec = Cow::Borrowed(value);
            }
            // Only explicit selectors seed candidate reuse from the input locks.
            if let Some(request) = lookup(name, &spec, config) {
                needed.insert(request.name().to_owned());
            }
            if group == "peerDependencies" && (!promoted || restore_peer) {
                if !exclude_self || own_name != Some(name) {
                    let optional = raw["peerDependenciesMeta"][name]["optional"] == true;
                    declarations
                        .peers
                        .insert(name.clone(), (spec.into_owned(), optional));
                }
                continue;
            }
            if group == "optionalDependencies" {
                if ignores_optional(name) {
                    removed_optional.insert(name.as_str());
                    continue;
                }
                // Bundling removes the snapshot edge, but the declaration still applies.
                effective_optional.insert(name.as_str());
            }
            if (group == "dependencies" && raw["optionalDependencies"].get(name).is_some())
                || bundle.is_some_and(|v| {
                    v == true || v.as_array().is_some_and(|a| a.iter().any(|v| v == name))
                })
            {
                continue;
            }
            let group = if promoted {
                match declarations.ordinary.get(name) {
                    Some(("optionalDependencies", _)) if promotes_over_optional => {
                        "optionalDependencies"
                    }
                    Some(("optionalDependencies", _)) => continue,
                    _ => "dependencies",
                }
            } else {
                group
            };
            declarations
                .ordinary
                .insert(name.clone(), (group, spec.into_owned()));
        }
    }
    // Optional metadata alone can declare a peer without a selector.
    let fallback = records_fallback_peers(config);
    for (name, meta) in graph::section(raw, "peerDependenciesMeta")
        .into_iter()
        .flatten()
    {
        // Bundled dependencies still supply the peer without a snapshot edge.
        let suppress_fallback = !fallback
            && raw["dependencies"].get(name).is_some()
            && !removed_dependencies.contains(name.as_str())
            && !removed_optional.contains(name.as_str());
        if meta["optional"] == true
            && raw["peerDependencies"].get(name).is_none()
            && (!exclude_self || own_name != Some(name))
            && !effective_optional.contains(name.as_str())
            && !suppress_fallback
        {
            declarations.peers.insert(name.clone(), ("*".into(), true));
        }
    }
    Ok(declarations)
}

fn request(error: LockfileError, lookup: Option<RegistryRequest>) -> LockfileError {
    match error {
        LockfileError::ResolutionRequired { path, message, .. } => {
            LockfileError::ResolutionRequired {
                path,
                message,
                request: lookup.map(Box::new),
            }
        }
        _ => error,
    }
}

fn lookup(name: &str, spec: &str, config: &WorkspaceConfig) -> Option<RegistryRequest> {
    let spec = if let Some(catalog) = spec.strip_prefix("catalog:") {
        config
            .catalogs
            .get(if catalog.is_empty() {
                "default"
            } else {
                catalog
            })?
            .get(name)?
    } else {
        spec
    };
    RegistryRequest::version(name, config.registry_specifier(spec))
}

pub(crate) fn need_version(
    path: &[&str],
    name: &str,
    spec: &str,
    config: &WorkspaceConfig,
    reason: String,
) -> LockfileError {
    let lookup = lookup(name, spec, config);
    if lookup.is_some() && config.resolution.pnpmfile_checksum.is_some() {
        return missing(
            path,
            format!(
                "{reason}; the project's pnpmfile may rewrite published dependency declarations"
            ),
        );
    }
    request(missing(path, reason), lookup)
}

fn effective_manifest<'a>(
    raw: &'a Value,
    config: &WorkspaceConfig,
) -> Result<Cow<'a, Value>, LockfileError> {
    let mut result = Cow::Borrowed(raw);
    let name = raw["name"].as_str().expect("validated published name");
    let version = raw["version"]
        .as_str()
        .expect("validated published version");
    for (selector, extension) in config
        .resolution
        .package_extensions
        .as_ref()
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if !manifest::parent_matches(selector, name, version) {
            continue;
        }
        for field in [
            "dependencies",
            "optionalDependencies",
            "peerDependencies",
            "peerDependenciesMeta",
        ] {
            if let Some(additions) = extension.get(field).and_then(Value::as_object) {
                let entries = result
                    .to_mut()
                    .as_object_mut()
                    .expect("published manifest")
                    .entry(field)
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .ok_or_else(|| missing(&[name, field], "expected mapping"))?;
                for (k, v) in additions {
                    entries.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }
    Ok(result)
}

pub(super) fn original(
    sources: &super::GraphSources<'_>,
    package: &Value,
    key: &str,
    group: &str,
    name: &str,
    reason: &str,
) -> Result<Option<String>, LockfileError> {
    declaration(
        sources.registry,
        sources.configuration,
        package,
        key,
        group,
        name,
        reason,
    )
}

pub(crate) fn declaration(
    registry: Option<&RegistryMetadata>,
    config: &WorkspaceConfig,
    package: &Value,
    key: &str,
    group: &str,
    name: &str,
    reason: &str,
) -> Result<Option<String>, LockfileError> {
    let key = pin::package_key(key);
    let path = if group == "peerDependencies" {
        vec!["packages", key, group, name]
    } else {
        vec!["snapshots", key, name]
    };
    let lookup = if config.resolution.pnpmfile_checksum.is_none() {
        RegistryRequest::manifest(key)
    } else {
        None
    };
    let publication = lookup
        .as_ref()
        .and_then(|_| registry?.releases.get(key))
        .ok_or_else(|| request(missing(&path, reason), lookup))?;
    if package["resolution"].get("integrity") != publication.resolution.get("integrity") {
        return Err(missing(
            &path,
            "published distribution integrity differs from the locked release",
        ));
    }
    let raw = effective_manifest(&publication.manifest, config)?;
    raw[group]
        .get(name)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| missing(&path, "invalid published dependency declaration"))
        })
        .transpose()
}

pub(super) fn catalog_version<'a>(
    config: &WorkspaceConfig,
    registry: Option<&'a RegistryMetadata>,
    name: &str,
    specifier: &str,
) -> Option<&'a str> {
    let (actual, range) = config.registry_request(name, specifier);
    registry?
        .selected_direct(actual, range)?
        .rsplit_once('@')
        .map(|(_, v)| v)
}

pub(crate) fn records(
    context: &MergeContext<'_>,
    metadata: &Value,
    docs: [&Value; 3],
    order: [usize; 3],
    additional: Option<&mut Vec<LockfileError>>,
) -> Result<Value, LockfileError> {
    let Some(registry) = context.registry else {
        return Ok(Value::Null);
    };
    let mut keys = registry.selected_releases().peekable();
    if keys.peek().is_none() {
        return Ok(Value::Null);
    }
    let config = context.configuration;
    if config.resolution.pnpmfile_checksum.is_some() || metadata.get("pnpmfileChecksum").is_some() {
        return Err(missing(
            &["pnpmfileChecksum"],
            "new releases require the project's effective hook output",
        ));
    }
    let rules = manifest::Overrides::new(metadata, config);
    let mut releases = Vec::new();
    let mut needed = BTreeSet::new();
    for key in keys.collect::<BTreeSet<_>>() {
        let publication = &registry.releases[key];
        let raw = effective_manifest(&publication.manifest, config)?;
        let declarations = published_declarations(&raw, key, &rules, config, &mut needed)?;
        releases.push((key, publication, raw, declarations));
    }
    let mut index: HashMap<&str, Vec<&str>> = HashMap::new();
    if !needed.is_empty() {
        for side in order {
            let compatible = crate::config::ResolutionCompatibility::new(
                &context.configurations[side].resolution,
                &config.resolution,
                docs[side],
            );
            for key in graph::section(docs[side], "snapshots")
                .into_iter()
                .flat_map(|m| m.keys())
            {
                if let Some((name, _)) = pin::package_key(key).rsplit_once('@')
                    && needed.contains(name)
                    && compatible.accepts(key)
                {
                    index.entry(name).or_default().push(key);
                }
            }
        }
    }
    let locked = |name: &str, spec: &str, request: &RegistryRequest| -> Option<String> {
        index.get(request.name())?.iter().find_map(|key| {
            let value = reference(name, key);
            (accepts_pin(name, spec, &value, metadata, Some(config))
                && desired_hash(key, metadata, [docs[0], docs[1], docs[2], &Value::Null])
                    == Some(patch::hash_in_key(key)))
            .then_some(value)
        })
    };
    let mut output = json!({"packages":{},"snapshots":{}});
    if let Some(overrides) = metadata.get("overrides") {
        output["overrides"] = overrides.clone();
    }
    if let Some(patches) = metadata.get("patchedDependencies") {
        output["patchedDependencies"] = patches.clone();
    }
    let mut missing_requests = super::MissingRequests::new(additional);
    let mut missing_peers = Vec::new();
    for (key, publication, raw, declarations) in releases {
        if docs
            .iter()
            .filter_map(|doc| doc["packages"][key]["resolution"].get("integrity"))
            .any(|integrity| Some(integrity) != publication.resolution.get("integrity"))
        {
            return Err(missing(
                &["packages", key, "resolution", "integrity"],
                "published distribution integrity differs from the locked release",
            ));
        }
        let mut package = package_record(&raw, &publication.resolution);
        for (name, (spec, optional)) in &declarations.peers {
            package["peerDependencies"][name] = spec.as_str().into();
            if *optional {
                package["peerDependenciesMeta"][name] = json!({"optional":true});
            }
        }
        // A dependency of the same name serves a peer the release declares.
        let peers = declarations
            .peers
            .iter()
            .filter(|(name, _)| !declarations.ordinary.contains_key(*name));
        let mut body = json!({});
        for (name, field, spec, peer) in declarations
            .ordinary
            .iter()
            .map(|(name, (group, spec))| (name, *group, spec, None))
            .chain(peers.map(|(name, (spec, optional))| {
                let field = if *optional {
                    "optionalDependencies"
                } else {
                    "dependencies"
                };
                (name, field, spec, Some(*optional))
            }))
        {
            let spec = config.registry_specifier(spec);
            let published = |req: &RegistryRequest| {
                let locator = registry.selected(req.name(), req.selector()).or_else(|| {
                    peer?;
                    // Another importer may supply this peer under a different
                    // selector. The workspace resolver applies its actual scope.
                    registry.peer_selections(req.name()).find(|key| {
                        accepts_pin(name, spec, &reference(name, key), metadata, Some(config))
                    })
                })?;
                let hash =
                    desired_hash(locator, metadata, [docs[0], docs[1], docs[2], &Value::Null])?;
                let locator =
                    hash.map_or_else(|| locator.clone(), |h| format!("{locator}(patch_hash={h})"));
                Some(reference(name, &locator))
            };
            let value = lookup(name, spec, config)
                .and_then(|request| locked(name, spec, &request).or_else(|| published(&request)));
            if let Some(optional) = peer
                && value.is_none()
                && (optional
                    || !config.resolution.lock.auto_install_peers
                    || context.manifests.values().any(|m| {
                        schema::DEPENDENCY_GROUPS
                            .iter()
                            .any(|g| m[g].get(name).is_some())
                    }))
            {
                continue;
            }
            let Some(value) = value else {
                let error = need_version(
                    &["snapshots", key, name],
                    name,
                    spec,
                    config,
                    format!("no locked resolution satisfies {spec}"),
                );
                if peer.is_some()
                    && matches!(
                        error,
                        LockfileError::ResolutionRequired {
                            request: Some(_),
                            ..
                        }
                    )
                {
                    // Ordinary dependencies, including aliases, can supply this peer on the next pass.
                    missing_peers.push(error);
                } else {
                    missing_requests.record(error)?;
                }
                continue;
            };
            body[field][name] = value.into();
        }
        let hash = desired_hash(key, metadata, [docs[0], docs[1], docs[2], &Value::Null])
            .ok_or_else(|| missing(&["patchedDependencies", key], "competing patch selectors"))?;
        let instance = hash.map_or_else(|| key.clone(), |h| format!("{key}(patch_hash={h})"));
        output["packages"][key] = package;
        output["snapshots"][instance] = body;
    }
    if missing_requests.first.is_none() {
        for error in missing_peers {
            missing_requests.record(error)?;
        }
    }
    missing_requests.finish()?;
    Ok(output)
}

fn package_record(raw: &Value, resolution: &Value) -> Value {
    let mut package = json!({"resolution":resolution});
    for field in ["os", "cpu", "libc", "deprecated"] {
        if let Some(v) = raw.get(field).filter(|v| {
            !v.is_null() && v.as_str() != Some("") && (field != "deprecated" || *v != false)
        }) {
            package[field] = v.clone();
        }
    }
    if let Some(engines) = graph::section(raw, "engines") {
        let values: Map<_, _> = engines
            .iter()
            .filter(|(_, v)| *v != "*")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !values.is_empty() {
            package["engines"] = values.into();
        }
    }
    if raw.get("bin").is_some_and(|v| {
        v.as_str().is_some_and(|v| !v.is_empty()) || v.as_object().is_some_and(|v| !v.is_empty())
    }) || raw["directories"]["bin"].is_string()
    {
        package["hasBin"] = true.into();
    }
    let bundle = raw
        .get("bundledDependencies")
        .or_else(|| raw.get("bundleDependencies"));
    if let Some(bundle) = bundle.filter(|v| v.is_array() || v.as_bool() == Some(true)) {
        package["bundledDependencies"] = bundle.clone();
    }
    package
}
