//! Add published releases to the set of graph sources under final workspace policy.
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
};

use serde_json::{Map, json};

use super::{MergeContext, accepts, alias, desired_patch, missing, reference};
use crate::{
    LockfileError, RegistryMetadata, RegistryRequest, Value, WorkspaceConfig, graph, manifest,
};

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
    RegistryRequest::version(name, spec)
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
    let key = graph::package_key(key);
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
    registry: Option<&'a RegistryMetadata>,
    name: &str,
    specifier: &str,
) -> Option<&'a str> {
    let (actual, range) = alias(specifier).unwrap_or((name, specifier));
    registry?
        .selected(actual, range)?
        .rsplit_once('@')
        .map(|(_, v)| v)
}

pub(crate) fn records(
    context: &MergeContext<'_>,
    metadata: &Value,
    docs: [&Value; 3],
    order: [usize; 3],
) -> Result<Value, LockfileError> {
    let Some(registry) = context.registry.filter(|r| !r.selections.is_empty()) else {
        return Ok(Value::Null);
    };
    let config = context.configuration;
    if config.resolution.pnpmfile_checksum.is_some() || metadata.get("pnpmfileChecksum").is_some() {
        return Err(missing(
            &["pnpmfileChecksum"],
            "new releases require the project's effective hook output",
        ));
    }
    let rules = manifest::Overrides::new(metadata, config.override_tie_break);
    let mut releases = Vec::new();
    let mut needed = BTreeSet::new();
    for key in registry
        .selections
        .values()
        .flat_map(|ranges| ranges.values())
        .collect::<BTreeSet<_>>()
    {
        let publication = &registry.releases[key];
        let raw = effective_manifest(&publication.manifest, config)?;
        for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
            for (name, value) in graph::section(&raw, group).into_iter().flatten() {
                if let Some(range) = value.as_str()
                    && let Some(spec) = rules.apply(&raw, ".", name, range)?
                    && let Some(request) = lookup(name, &spec, config)
                {
                    needed.insert(request.name().to_owned());
                }
            }
        }
        releases.push((key, publication, raw));
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
                if let Some((name, _)) = graph::package_key(key).rsplit_once('@')
                    && needed.contains(name)
                    && compatible.accepts(key)
                {
                    index.entry(name).or_default().push(key);
                }
            }
        }
    }
    let locked = |name: &str, spec: &str| -> Option<String> {
        let request = lookup(name, spec, config)?;
        let actual = request.name();
        index.get(actual)?.iter().find_map(|key| {
            let value = reference(name, key);
            (accepts(name, spec, &value, metadata)
                && desired_patch(key, metadata, [docs[0], docs[1], docs[2], &Value::Null])
                    == Some(crate::peer::patch(key)))
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
    for (key, publication, raw) in releases {
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
        let bundle = raw
            .get("bundledDependencies")
            .or_else(|| raw.get("bundleDependencies"));
        let mut body = json!({});
        for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
            let Some(deps) = raw.get(group) else { continue };
            for (name, spec) in deps
                .as_object()
                .ok_or_else(|| missing(&[key, group], "invalid published dependency mapping"))?
            {
                if group == "dependencies" && raw["optionalDependencies"].get(name).is_some() {
                    continue;
                }
                if group == "optionalDependencies"
                    && config
                        .resolution
                        .ignored_optional_dependencies
                        .as_ref()
                        .and_then(Value::as_array)
                        .is_some_and(|a| a.iter().any(|v| v == name))
                {
                    continue;
                }
                if group != "peerDependencies"
                    && bundle.is_some_and(|v| {
                        v.as_bool() == Some(true)
                            || v.as_array()
                                .is_some_and(|arr| arr.iter().any(|x| x == name))
                    })
                {
                    continue;
                }
                let spec = spec.as_str().ok_or_else(|| {
                    missing(
                        &[key, group, name],
                        "invalid published dependency declaration",
                    )
                })?;
                let Some(spec) = rules.apply(&raw, ".", name, spec)? else {
                    continue;
                };
                let is_peer =
                    group == "peerDependencies" && spec.parse::<node_semver::Range>().is_ok();
                let optional = raw["peerDependenciesMeta"][name]["optional"] == true;
                if is_peer {
                    package["peerDependencies"][name] = spec.as_ref().into();
                    if optional {
                        package["peerDependenciesMeta"][name] = json!({"optional":true});
                    }
                    if raw["dependencies"].get(name).is_some()
                        || raw["optionalDependencies"].get(name).is_some()
                    {
                        continue;
                    }
                }
                let published = || {
                    let req = lookup(name, &spec, config)?;
                    let locator = registry.selected(req.name(), req.selector()).or_else(|| {
                        if !is_peer {
                            return None;
                        }
                        // Another importer may supply this peer under a different
                        // selector. The workspace resolver applies its actual scope.
                        registry
                            .selections
                            .get(req.name())?
                            .values()
                            .find(|key| accepts(name, &spec, &reference(name, key), metadata))
                    })?;
                    let hash = desired_patch(
                        locator,
                        metadata,
                        [docs[0], docs[1], docs[2], &Value::Null],
                    )?;
                    let locator = hash
                        .map_or_else(|| locator.clone(), |h| format!("{locator}(patch_hash={h})"));
                    Some(reference(name, &locator))
                };
                let value = locked(name, &spec).or_else(published);
                if is_peer
                    && value.is_none()
                    && (optional
                        || !config.resolution.lock.auto_install_peers
                        || context
                            .manifests
                            .values()
                            .any(|m| manifest::GROUPS.iter().any(|g| m[g].get(name).is_some())))
                {
                    continue;
                }
                let value = value.ok_or_else(|| {
                    need_version(
                        &["snapshots", key, name],
                        name,
                        &spec,
                        config,
                        format!("no locked resolution satisfies {spec}"),
                    )
                })?;
                let field = if is_peer && optional {
                    "optionalDependencies"
                } else if group == "peerDependencies" {
                    "dependencies"
                } else {
                    group
                };
                body[field][name] = value.into();
            }
        }
        let hash = desired_patch(key, metadata, [docs[0], docs[1], docs[2], &Value::Null])
            .ok_or_else(|| missing(&["patchedDependencies", key], "competing patch selectors"))?;
        let instance = hash.map_or_else(|| key.clone(), |h| format!("{key}(patch_hash={h})"));
        output["packages"][key] = package;
        output["snapshots"][instance] = body;
    }
    Ok(output)
}

fn package_record(raw: &Value, resolution: &Value) -> Value {
    let mut package = json!({"resolution":resolution});
    for field in ["os", "cpu", "libc", "deprecated"] {
        if let Some(v) = raw
            .get(field)
            .filter(|v| !v.is_null() && v.as_str() != Some(""))
        {
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
