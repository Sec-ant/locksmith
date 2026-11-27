//! Construct and check project-wide settings, catalogs, checksums and patch records.
use super::MergeContext;
use crate::error::missing;
use crate::{LockfileError, Value, WorkspaceConfig, catalog, graph, manifest, patch, pin, schema};
use serde_json::json;

pub(super) fn check_merge_settings(
    doc: &Value,
    config: &WorkspaceConfig,
) -> Result<(), LockfileError> {
    if doc.get("settings").is_some_and(|value| !value.is_object()) {
        return Err(missing(&["settings"], "expected mapping"));
    }
    match config.pnpm_version.as_ref() {
        None => config.check_recorded_peer_suffix(doc)?,
        Some(version) if version >= &node_semver::Version::from((12, 15, 0)) => {
            config.check_recorded_settings(doc)?;
        }
        // For older runners, supplied settings can include values from files that pnpm ignores.
        Some(_) => {}
    }
    if let Some(field) = config.frozen_settings_mismatch(doc) {
        return Err(missing(
            &["settings", field],
            "lockfile setting differs from the workspace; regenerate the lockfile with pnpm",
        ));
    }
    Ok(())
}

pub(crate) fn check_repair_configuration(
    doc: &Value,
    metadata: &Value,
    cfg: &WorkspaceConfig,
) -> Result<(), LockfileError> {
    let mismatch = |field| {
        missing(
            &[field],
            "workspace configuration differs from the lock; its effects extend beyond a broken record",
        )
    };
    if cfg.read_lock_settings(doc)? != cfg.read_lock_settings(metadata)? {
        return Err(mismatch("settings"));
    }
    cfg.check_recorded_settings(doc)?;
    if !manifest::same_overrides(doc, metadata) {
        return Err(mismatch("overrides"));
    }
    if !patch::records_match(doc, metadata.get("patchedDependencies")) {
        return Err(mismatch("patchedDependencies"));
    }
    if !crate::checksum::extensions_match(
        cfg,
        doc.get("packageExtensionsChecksum")
            .filter(|hash| hash.is_string()),
    ) {
        return Err(mismatch("packageExtensionsChecksum"));
    }
    if !crate::checksum::hooks_match(
        cfg.resolution.pnpmfile_checksum.as_ref(),
        doc.get("pnpmfileChecksum"),
    ) {
        return Err(mismatch("pnpmfileChecksum"));
    }
    if !cfg.matches_optional_header_for_repair(doc)? {
        return Err(mismatch("ignoredOptionalDependencies"));
    }
    Ok(())
}

pub(crate) fn resolve(
    docs: [&Value; 3],
    order: [usize; 3],
    settings_source: usize,
    context: &MergeContext<'_>,
) -> Result<Value, LockfileError> {
    let structural = [
        "importers",
        "packages",
        "snapshots",
        "catalogs",
        "overrides",
        "patchedDependencies",
        "time",
    ];
    let mut output = Value::Object(
        docs[order[0]]
            .as_object()
            .expect("parsed document")
            .iter()
            .filter(|(field, _)| !structural.contains(&field.as_str()))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect(),
    );
    let config = context.configuration;
    if settings_source != order[0] {
        output["settings"] = docs[settings_source]
            .get("settings")
            .cloned()
            .unwrap_or_else(|| json!({}));
    }
    let settings = output
        .as_object_mut()
        .expect("project metadata")
        .entry("settings")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| missing(&["settings"], "expected mapping"))?;
    let recorded = config.recorded_settings();
    settings.insert(
        "autoInstallPeers".into(),
        json!(recorded.auto_install_peers),
    );
    if recorded.project_scoped_optional_peers {
        settings.insert("projectScopedOptionalPeers".into(), json!(true));
    } else {
        settings.remove("projectScopedOptionalPeers");
    }
    if recorded.dedupe_peers {
        settings.insert("dedupePeers".into(), json!(true));
    } else {
        settings.remove("dedupePeers");
    }
    settings.insert(
        "excludeLinksFromLockfile".into(),
        json!(recorded.exclude_links_from_lockfile),
    );
    for (field, lock_field, value, source_values) in [
        (
            "packageExtensions",
            "packageExtensionsChecksum",
            &config.resolution.package_extensions,
            context
                .configurations
                .map(|c| &c.resolution.package_extensions),
        ),
        (
            "ignoredOptionalDependencies",
            "ignoredOptionalDependencies",
            &config.resolution.ignored_optional_dependencies,
            context
                .configurations
                .map(|c| &c.resolution.ignored_optional_dependencies),
        ),
        (
            "pnpmfileChecksum",
            "pnpmfileChecksum",
            &config.resolution.pnpmfile_checksum,
            context
                .configurations
                .map(|c| &c.resolution.pnpmfile_checksum),
        ),
    ] {
        let source = order.iter().find(|&&side| {
            if source_values[side] != value {
                return false;
            }
            if field == "packageExtensions"
                && let Some(encoding) = config.extension_checksum
                && let Some(hash) = docs[side].get(lock_field).and_then(Value::as_str)
            {
                return (encoding == crate::ExtensionChecksum::Sha256)
                    == hash.starts_with("sha256-");
            }
            true
        });
        let recorded = if let Some(source) = source {
            docs[*source].get(lock_field).cloned()
        } else if field == "packageExtensions" {
            match value
                .as_ref()
                .filter(|v| !v.is_null() && v.as_object().is_none_or(|m| !m.is_empty()))
            {
                None => None,
                Some(value) => Some(Value::String(
                    order
                        .iter()
                        .find_map(|side| {
                            crate::checksum::extensions(
                                value,
                                source_values[*side].as_ref()?,
                                docs[*side].get(lock_field)?.as_str()?,
                                config.extension_checksum,
                            )
                        })
                        .ok_or_else(|| {
                            missing(
                                &[field],
                                "no source checksum verifies the configuration encoding",
                            )
                        })?,
                )),
            }
        } else {
            return Err(missing(
                &[field],
                "no input contains metadata for the final configuration",
            ));
        };
        if let Some(value) = recorded {
            output[lock_field] = value;
        } else {
            output.as_object_mut().unwrap().remove(lock_field);
        }
    }
    let catalogs = &config.catalogs;
    let mut versions = std::array::from_fn::<_, 3, _>(|_| None);
    for (catalog, entries) in catalogs {
        for (name, specifier) in entries {
            let specifier = specifier.as_str();
            let entry = order
                .iter()
                .find_map(|side| {
                    let entry = docs[*side].get("catalogs")?.get(catalog)?.get(name)?;
                    let version = entry.get("version")?.as_str()?;
                    let recorded = context.configurations[*side]
                        .registry_specifier(entry["specifier"].as_str()?);
                    let effective = config.registry_specifier(specifier);
                    if pin::alias(recorded).map(|(n, _)| n) != pin::alias(effective).map(|(n, _)| n)
                    {
                        return None;
                    }
                    catalog::version_fits(effective, version, Some(recorded))
                        .then(|| json!({"specifier":specifier,"version":version}))
                })
                .or_else(|| {
                    let (actual, range) = config.registry_request(name, specifier);
                    let range = range.parse::<node_semver::Range>().ok()?;
                    order.iter().find_map(|&side| {
                        let index = versions[side].get_or_insert_with(|| {
                            let mut index = std::collections::HashMap::<_, Vec<_>>::new();
                            for key in graph::section(docs[side], "packages")
                                .into_iter()
                                .flat_map(|m| m.keys())
                            {
                                if let Some((name, version)) = key.rsplit_once('@') {
                                    index.entry(name).or_default().push(version);
                                }
                            }
                            index
                        });
                        let version = index
                            .get(actual)?
                            .iter()
                            .filter_map(|&v| {
                                let parsed = v.parse::<node_semver::Version>().ok()?;
                                parsed.satisfies(&range).then_some((parsed, v))
                            })
                            .max_by(|(a, _), (b, _)| a.cmp(b))
                            .map(|(_, v)| v)?;
                        Some(json!({"specifier":specifier,"version":version}))
                    })
                })
                .or_else(|| {
                    super::published::catalog_version(config, context.registry, name, specifier)
                        .map(|version| json!({"specifier":specifier,"version":version}))
                });
            if let Some(entry) = entry {
                if output.get("catalogs").is_none() {
                    output["catalogs"] = json!({});
                }
                if output["catalogs"].get(catalog).is_none() {
                    output["catalogs"][catalog] = json!({});
                }
                output["catalogs"][catalog][name] = entry;
            }
        }
    }
    if let Some(rules) = manifest::effective_overrides(config, context.manifests.get("."))? {
        output["overrides"] = rules;
    }
    if let Some(patches) = patch::records(config, context.patches, config.patch_encoding)? {
        output["patchedDependencies"] = patches;
    }
    Ok(output)
}

/// pnpm records publication times for direct dependency locators only.
pub(crate) fn publication_times<const N: usize>(
    project: &Value,
    sources: [&Value; N],
    config: Option<&crate::WorkspaceConfig>,
) -> Option<Value> {
    let sources = sources.map(|doc| graph::section(doc, "time"));
    let mut times = serde_json::Map::new();
    if sources.iter().flatten().all(|times| times.is_empty()) {
        return sources
            .iter()
            .any(Option::is_some)
            .then(|| Value::Object(times));
    }
    let major = config
        .and_then(|c| c.pnpm_version.as_ref())
        .map(|v| v.major());
    let snapshots = graph::section(project, "snapshots");
    for (_, importer) in graph::section(project, "importers").into_iter().flatten() {
        for group in schema::DEPENDENCY_GROUPS {
            for (name, dep) in graph::section(importer, group).into_iter().flatten() {
                let Some(version) = dep["version"].as_str().filter(|v| !v.starts_with("link:"))
                else {
                    continue;
                };
                let key = graph::snapshot_key(name, version, snapshots);
                let key = pin::package_key(&key);
                let reference = pin::package_key(version);
                // pnpm 9 and 10 use /name@version, or the reference itself for files and paths.
                let legacy = if reference.starts_with("file:") || reference.contains('/') {
                    reference.to_owned()
                } else {
                    format!("/{name}@{reference}")
                };
                let keys: &[&str] = match major {
                    Some(..=10) => &[&legacy],
                    Some(_) => &[key],
                    None => &[key, &legacy],
                };
                if let Some((key, time)) = sources
                    .iter()
                    .flatten()
                    .find_map(|source| keys.iter().find_map(|key| source.get_key_value(*key)))
                {
                    times.insert(key.clone(), time.clone());
                }
            }
        }
    }
    Some(Value::Object(times))
}

/// pnpm records a catalog snapshot only for a direct dependency resolved
/// through that catalog. An override may replace the catalog specifier.
pub(super) fn retain_used_catalogs(output: &mut Value) {
    let Some(mut catalogs) = output.as_object_mut().expect("project").remove("catalogs") else {
        return;
    };
    let mut used = std::collections::HashSet::new();
    for (_, importer) in graph::section(output, "importers").into_iter().flatten() {
        for group in schema::DEPENDENCY_GROUPS {
            for (name, dep) in graph::section(importer, group).into_iter().flatten() {
                if let Some(catalog) = dep["specifier"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("catalog:"))
                {
                    used.insert((
                        if catalog.is_empty() {
                            "default"
                        } else {
                            catalog
                        },
                        name.as_str(),
                    ));
                }
            }
        }
    }
    let groups = catalogs.as_object_mut().expect("resolved catalogs");
    groups.retain(|catalog, entries| {
        let entries = entries.as_object_mut().expect("resolved catalog entries");
        entries.retain(|name, _| used.contains(&(catalog.as_str(), name.as_str())));
        !entries.is_empty()
    });
    if !groups.is_empty() {
        output["catalogs"] = catalogs;
    }
}
