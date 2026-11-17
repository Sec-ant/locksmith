use std::collections::BTreeSet;

use serde_json::json;

use crate::{Conflict, ConflictKind, DocumentKind, Lockfile, LockfileError, Value, graph};

/// Exact importer paths relative to the workspace root; no filesystem access or
/// glob expansion. Metadata is retained from the baseline unless explicitly listed.
#[derive(Debug, Clone, Default)]
pub struct GraftOptions {
    pub importers: BTreeSet<String>,
    /// Whole top-level fields to take from the donor, including deletion.
    /// For example: `catalogs`, `settings`, or `patchedDependencies`.
    /// Structural fields (`lockfileVersion`, `importers`, `packages`, `snapshots`)
    /// are governed by the graft algorithm and cannot be listed here.
    pub metadata: BTreeSet<String>,
}

impl GraftOptions {
    pub fn new(importers: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            importers: importers
                .into_iter()
                .map(|path| {
                    let path = path.as_ref().trim_start_matches("./").trim_end_matches('/');
                    if path.is_empty() {
                        ".".into()
                    } else {
                        path.into()
                    }
                })
                .collect(),
            metadata: BTreeSet::new(),
        }
    }
}

/// Transplant complete selected importers and their dependency closures.
///
/// Missing selected importers in the donor are deletions. Unselected importers,
/// their shared records, the environment document, and metadata come from `base`.
/// Conflicting shared records are returned as structured errors. Workspace links
/// are followed; a linked project that changes must also be explicitly selected.
/// Records made unreachable by this graft are removed. Pre-existing unreachable
/// records remain unchanged, so an unrelated cleanup does not inflate the diff.
pub fn graft(
    base: &Lockfile,
    donor: &Lockfile,
    options: &GraftOptions,
) -> Result<Lockfile, LockfileError> {
    if options.importers.is_empty() {
        return Err(LockfileError::EmptySelection);
    }
    for field in &options.metadata {
        if ["lockfileVersion", "importers", "packages", "snapshots"].contains(&field.as_str()) {
            return Err(LockfileError::InvalidMetadataSelection(field.clone()));
        }
    }
    let empty = json!({"lockfileVersion": "9.0", "importers": {}});
    let baseline = base.project().unwrap_or(&empty);
    let current = donor.project().unwrap_or(&empty);
    let base_keys = graph::keys(baseline, "importers");
    let donor_keys = graph::keys(current, "importers");
    for key in &options.importers {
        if !base_keys.contains(key) && !donor_keys.contains(key) {
            return Err(LockfileError::UnknownImporter(key.clone()));
        }
    }
    let others = &base_keys - &options.importers;
    let targets = &donor_keys & &options.importers;
    let protected = graph::closure(baseline, DocumentKind::Project, &others)?;
    let selected = graph::closure(current, DocumentKind::Project, &targets)?;
    let mut conflicts = Vec::new();
    for key in &selected.importers {
        if !options.importers.contains(key) || protected.importers.contains(key) {
            protect(baseline, current, "importers", key, &mut conflicts);
        }
    }
    for key in selected.snapshots.intersection(&protected.snapshots) {
        protect(baseline, current, "snapshots", key, &mut conflicts);
    }
    for key in selected.packages.intersection(&protected.packages) {
        protect(baseline, current, "packages", key, &mut conflicts);
    }
    // Deleting or changing a workspace project used by another project also
    // changes that project's dependency graph, even when the link itself is equal.
    for key in options.importers.intersection(&protected.importers) {
        if !selected.importers.contains(key) {
            protect(baseline, current, "importers", key, &mut conflicts);
        }
    }
    if !conflicts.is_empty() {
        return Err(LockfileError::Conflicts(conflicts));
    }

    let mut output = baseline.clone();
    for key in &options.importers {
        replace_entry(&mut output, current, "importers", key);
    }
    for key in &selected.snapshots {
        replace_entry(&mut output, current, "snapshots", key);
    }
    for key in &selected.packages {
        replace_entry(&mut output, current, "packages", key);
    }
    for field in &options.metadata {
        let root = output
            .as_object_mut()
            .expect("parsed document is a mapping");
        if let Some(value) = current.get(field) {
            root.insert(field.clone(), value.clone());
        } else {
            root.remove(field);
        }
    }
    complete(
        &mut output,
        DocumentKind::Project,
        Some(baseline),
        &mut Vec::new(),
    )?;
    let mut documents = base.documents.clone();
    documents.insert(DocumentKind::Project, std::sync::Arc::new(output));
    if let Some(environment) = base.environment() {
        let issues = graph::validate(environment, DocumentKind::Environment);
        if !issues.is_empty() {
            return Err(LockfileError::InvalidGraph(issues));
        }
    }
    Ok(Lockfile::with_documents(documents, &[base, donor]))
}

/// Assemble a graph from independently selected importer edges. The large
/// input graphs stay borrowed until their selected records are copied once.
pub(crate) fn from_edges(
    base: &Lockfile,
    donor: &Lockfile,
    importers: Value,
    retained: &Value,
    adopted: &Value,
) -> Result<Lockfile, LockfileError> {
    let baseline = base.project().expect("converge checked project document");
    let current = donor.project().expect("converge checked project document");
    let protected = graph::closure_from_importers(baseline, retained)?;
    let selected = graph::closure_from_importers(current, adopted)?;
    let mut conflicts = Vec::new();
    for key in selected.snapshots.intersection(&protected.snapshots) {
        protect(baseline, current, "snapshots", key, &mut conflicts);
    }
    for key in selected.packages.intersection(&protected.packages) {
        protect(baseline, current, "packages", key, &mut conflicts);
    }
    if !conflicts.is_empty() {
        return Err(LockfileError::Conflicts(conflicts));
    }
    let mut output = baseline.clone();
    output["importers"] = importers;
    for key in &selected.snapshots {
        replace_entry(&mut output, current, "snapshots", key);
    }
    for key in &selected.packages {
        replace_entry(&mut output, current, "packages", key);
    }
    complete(
        &mut output,
        DocumentKind::Project,
        Some(baseline),
        &mut conflicts,
    )?;
    let mut documents = base.documents.clone();
    documents.insert(DocumentKind::Project, std::sync::Arc::new(output));
    Ok(Lockfile::with_documents(documents, &[base, donor]))
}

/// Shared completion policy for convergence, grafts and three-way merges.
/// Only reachable graph conflicts survive pruning; other conflicts remain atomic.
pub(crate) fn complete(
    document: &mut Value,
    kind: DocumentKind,
    baseline: Option<&Value>,
    conflicts: &mut Vec<Conflict>,
) -> Result<(), LockfileError> {
    if conflicts.iter().all(|conflict| {
        conflict.path.len() == 2 && ["packages", "snapshots"].contains(&conflict.path[0].as_str())
    }) {
        if let Some(baseline) = baseline {
            graph::prune_scoped(document, kind, baseline)?;
        } else {
            graph::prune(document, kind)?;
        }
        conflicts.retain(|conflict| {
            document
                .get(&conflict.path[0])
                .and_then(|map| map.get(&conflict.path[1]))
                .is_some()
        });
    }
    Ok(())
}

fn protect(base: &Value, donor: &Value, section: &str, key: &str, conflicts: &mut Vec<Conflict>) {
    let old = base.get(section).and_then(|map| map.get(key));
    let new = donor.get(section).and_then(|map| map.get(key));
    if old != new {
        conflicts.push(Conflict {
            document: DocumentKind::Project,
            path: vec![section.into(), key.into()],
            kind: ConflictKind::SharedDependency,
            base: old.cloned(),
            ours: old.cloned(),
            theirs: new.cloned(),
        });
    }
}

fn replace_entry(output: &mut Value, donor: &Value, section: &str, key: &str) {
    let new = donor.get(section).and_then(|map| map.get(key));
    let root = output
        .as_object_mut()
        .expect("parsed document is a mapping");
    if let Some(value) = new {
        root.entry(section)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("parsed section is a mapping")
            .insert(key.into(), value.clone());
    } else if let Some(map) = root.get_mut(section).and_then(Value::as_object_mut) {
        map.remove(key);
    }
}
