//! Repair an existing lock without preferring an earlier branch's legal pins.
mod evidence;
mod importers;
mod peers;

use evidence::Evidence;

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    DocumentKind, Lockfile, LockfileError, RegistryMetadata, Value, WorkspaceConfig, graph,
    manifest,
    reconcile::{self, missing},
    source::edit,
};

/// Final workspace inputs and optional evidence for missing records.
/// Parents are (lock, effective configuration) pairs. Their order does not
/// authorize replacing a valid pin in the current lock.
pub struct RepairContext<'a> {
    pub manifests: &'a BTreeMap<String, Value>,
    pub configuration: &'a WorkspaceConfig,
    pub patches: &'a BTreeMap<String, String>,
    pub parents: &'a [(&'a Lockfile, &'a WorkspaceConfig)],
    pub registry: Option<&'a RegistryMetadata>,
}

/// Repair the supplied YAML against the final workspace. A healthy input is
/// returned byte-for-byte. Missing or ambiguous evidence produces an error.
pub fn repair(text: &str, context: &RepairContext<'_>) -> Result<Lockfile, LockfileError> {
    let lock = edit::parse_for_repair(text)?;
    let doc = lock
        .project()
        .ok_or_else(|| missing(&[], "missing project document"))?;
    let root = context
        .manifests
        .get(".")
        .ok_or_else(|| missing(&["manifests", "."], "the root manifest is required"))?;
    if let Some(env) = lock.environment() {
        let issues = graph::validate(env, DocumentKind::Environment);
        if !issues.is_empty() {
            return Err(LockfileError::InvalidGraph(issues));
        }
        if let (Some(pin), Some(manager)) = (
            env["importers"]["."]["packageManagerDependencies"]["pnpm"]["specifier"].as_str(),
            root["packageManager"]
                .as_str()
                .and_then(|v| v.strip_prefix("pnpm@")),
        ) && manager.split('+').next() != Some(pin)
        {
            return Err(missing(
                &["packageManagerDependencies", "pnpm"],
                "bootstrap graph does not match the declared package manager",
            ));
        }
    }
    let mut evidence = Evidence::new(&lock, context);
    let current = (&lock, context.configuration);
    let sources = [
        current,
        context.parents.first().copied().unwrap_or(current),
        context.parents.get(1).copied().unwrap_or(current),
    ];
    let docs = sources.map(|(lock, _)| {
        lock.project()
            .ok_or_else(|| missing(&["parents"], "expected a project lockfile"))
    });
    let [d, a, b] = docs;
    let metadata = reconcile::metadata::resolve(
        [d?, a?, b?],
        [0, 1, 2],
        &evidence.merge_context(sources.map(|(_, cfg)| cfg)),
    )?;
    check_configuration(doc, &metadata, context.configuration)?;
    let mut result = importers::repair(doc, &metadata, &mut evidence)?;
    let roots = context.manifests.keys().cloned().collect();
    let mut checked_extensions = false;
    loop {
        if graph::closure(&result, DocumentKind::Project, &roots).is_err() {
            fill_missing(&mut result, &mut evidence)?;
            graph::closure(&result, DocumentKind::Project, &roots)?;
        }
        if !checked_extensions {
            checked_extensions = true;
            let plan = crate::extensions::plan(&result, context.configuration, None)?;
            if !plan.is_empty() {
                plan.apply(result.to_mut());
                continue;
            }
        }
        if matches!(result, Cow::Borrowed(_)) {
            break;
        }
        let edits = peers::affected(&result, &mut evidence)?;
        if edits.is_empty() {
            break;
        }
        for (id, group, name, pin) in edits {
            result.to_mut()["importers"][id][group][name]["version"] = Value::String(pin);
        }
    }
    drop(evidence);
    match result {
        Cow::Borrowed(_) => Ok(lock),
        Cow::Owned(doc) => edit::project(&lock, &doc),
    }
}

fn check_configuration(
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
    if crate::LockSettings::from_document(doc) != crate::LockSettings::from_document(metadata) {
        return Err(mismatch("settings"));
    }
    if !manifest::same_overrides(doc, metadata) {
        return Err(mismatch("overrides"));
    }
    if doc.get("patchedDependencies") != metadata.get("patchedDependencies") {
        return Err(mismatch("patchedDependencies"));
    }
    let ext = cfg
        .resolution
        .package_extensions
        .as_ref()
        .filter(|v| v.as_object().is_none_or(|m| !m.is_empty()));
    match (ext, doc["packageExtensionsChecksum"].as_str()) {
        (None, None) => (),
        (Some(ext), Some(hash))
            if crate::checksum::extensions(ext, ext, hash, cfg.extension_checksum).as_deref()
                == Some(hash) => {}
        _ => return Err(mismatch("packageExtensionsChecksum")),
    }
    if !crate::checksum::hooks_match(
        cfg.resolution.pnpmfile_checksum.as_ref(),
        doc.get("pnpmfileChecksum"),
    ) {
        return Err(mismatch("pnpmfileChecksum"));
    }
    if cfg.resolution.ignored_optional_dependencies.as_ref()
        != doc.get("ignoredOptionalDependencies")
    {
        return Err(mismatch("ignoredOptionalDependencies"));
    }
    Ok(())
}

fn fill_missing(
    doc: &mut Cow<'_, Value>,
    evidence: &mut Evidence<'_>,
) -> Result<(), LockfileError> {
    let mut pending = BTreeSet::new();
    let mut seen = std::collections::HashSet::new();
    for (id, record) in graph::section(doc, "importers").into_iter().flatten() {
        enqueue(doc, "importers", id, record, &mut pending)?;
    }
    while let Some(key) = pending.pop_first() {
        if seen.contains(&key) {
            continue;
        }
        if doc["snapshots"].get(&key).is_none() {
            doc.to_mut()["snapshots"][&key] = evidence.record("snapshots", &key)?;
        }
        let pkg = graph::package_key(&key);
        if doc["packages"].get(pkg).is_none() {
            doc.to_mut()["packages"][pkg] = evidence.record("packages", pkg)?;
        }
        enqueue(
            doc,
            "snapshots",
            &key,
            &doc["snapshots"][&key],
            &mut pending,
        )?;
        seen.insert(key);
    }
    Ok(())
}

fn enqueue(
    doc: &Value,
    section: &str,
    id: &str,
    record: &Value,
    pending: &mut BTreeSet<String>,
) -> Result<(), LockfileError> {
    let mut issues = Vec::new();
    let snapshots = graph::section(doc, "snapshots");
    for (name, version) in graph::references(
        record,
        DocumentKind::Project,
        section,
        id,
        true,
        &mut issues,
    ) {
        if version.starts_with("link:") {
            continue;
        }
        let key = graph::snapshot_key(name, version, snapshots);
        let key = if !snapshots.is_some_and(|m| m.contains_key(&key))
            && crate::RegistryRequest::manifest(version).is_some()
        {
            version.to_owned()
        } else {
            key
        };
        pending.insert(key);
    }
    if !issues.is_empty() {
        return Err(LockfileError::InvalidGraph(issues));
    }
    Ok(())
}
