use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde_json::Map;

use crate::{DocumentKind, LockfileError, ValidationIssue, Value};

pub(crate) const DEPENDENCIES: [&str; 5] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "configDependencies",
    "packageManagerDependencies",
];

/// Records reachable from one or more importers. Snapshot identities retain
/// complete peer and patch suffixes; linked workspace projects are included.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DependencyClosure {
    pub importers: BTreeSet<String>,
    pub snapshots: BTreeSet<String>,
    pub packages: BTreeSet<String>,
}

impl DependencyClosure {
    /// Packages installed as multiple distinct peer/patch instances in this
    /// closure. Multiple instances can be legitimate; this is inspection data,
    /// not an automatic error or permission to collapse them.
    pub fn variants(&self) -> BTreeMap<&str, Vec<&str>> {
        let mut variants: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for snapshot in &self.snapshots {
            variants
                .entry(package_key(snapshot))
                .or_default()
                .push(snapshot);
        }
        variants.retain(|_, entries| entries.len() > 1);
        variants
    }
}

pub(crate) fn section<'a>(document: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    document.get(key).and_then(Value::as_object)
}

pub(crate) fn keys(document: &Value, key: &str) -> BTreeSet<String> {
    section(document, key)
        .into_iter()
        .flatten()
        .map(|(key, _)| key.clone())
        .collect()
}

pub(crate) fn package_key(snapshot: &str) -> &str {
    snapshot.split_once('(').map_or(snapshot, |(key, _)| key)
}

/// pnpm writes npm aliases and remote/file references as either a full key or
/// a version relative to the dependency name. Peer and patch suffixes stay intact.
pub(crate) fn snapshot_key(
    name: &str,
    reference: &str,
    snapshots: Option<&Map<String, Value>>,
) -> String {
    let mut named = String::with_capacity(name.len() + 1 + reference.len());
    named.push_str(name);
    named.push('@');
    named.push_str(reference);
    if snapshots.is_some_and(|map| map.contains_key(reference) && !map.contains_key(&named)) {
        reference.into()
    } else {
        named
    }
}

fn issue(kind: DocumentKind, path: &[&str], message: impl Into<String>) -> ValidationIssue {
    ValidationIssue {
        document: kind,
        path: path.iter().map(|s| (*s).into()).collect(),
        message: message.into(),
    }
}

pub(crate) fn references<'a>(
    value: &'a Value,
    kind: DocumentKind,
    section: &str,
    key: &str,
    include_dev: bool,
    issues: &mut Vec<ValidationIssue>,
) -> Vec<(&'a str, &'a str)> {
    let mut result = Vec::new();
    if !value.is_object() {
        issues.push(issue(kind, &[section, key], "expected a mapping"));
        return result;
    }
    for group in DEPENDENCIES {
        if (!include_dev && group == "devDependencies")
            || (section == "snapshots"
                && !["dependencies", "optionalDependencies"].contains(&group))
        {
            continue;
        }
        let Some(dependencies) = value.get(group) else {
            continue;
        };
        let Some(dependencies) = dependencies.as_object() else {
            issues.push(issue(
                kind,
                &[section, key, group],
                "expected a dependency mapping",
            ));
            continue;
        };
        for (name, dependency) in dependencies {
            let reference = if section == "importers" {
                if dependency
                    .get("specifier")
                    .and_then(Value::as_str)
                    .is_none()
                {
                    issues.push(issue(
                        kind,
                        &[section, key, group, name, "specifier"],
                        "expected a string",
                    ));
                }
                dependency.get("version").and_then(Value::as_str)
            } else {
                dependency.as_str()
            };
            match reference {
                Some(reference) if !reference.is_empty() => result.push((name.as_str(), reference)),
                _ => issues.push(issue(
                    kind,
                    &[section, key, group, name],
                    "expected a nonempty resolved version",
                )),
            }
        }
    }
    result
}

pub(crate) fn linked_importer(from: &str, target: &str) -> String {
    let mut segments = Vec::new();
    for segment in from.split('/').chain(target.split('/')) {
        match segment {
            "" | "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        ".".into()
    } else {
        segments.join("/")
    }
}

pub(crate) fn closure(
    document: &Value,
    kind: DocumentKind,
    roots: &BTreeSet<String>,
) -> Result<DependencyClosure, LockfileError> {
    closure_with_snapshots(
        document,
        document.get("importers"),
        kind,
        roots,
        &BTreeSet::new(),
    )
}

/// Traverse references from a caller-selected set of importer edges while
/// borrowing the package graph. Used to combine baseline and candidate edges.
pub(crate) fn closure_from_importers(
    document: &Value,
    importers: &Value,
) -> Result<DependencyClosure, LockfileError> {
    closure_from_edges(document, importers, &BTreeSet::new())
}

pub(crate) fn closure_from_edges(
    document: &Value,
    importers: &Value,
    snapshots: &BTreeSet<String>,
) -> Result<DependencyClosure, LockfileError> {
    let roots = importers
        .as_object()
        .expect("importer mapping")
        .keys()
        .cloned()
        .collect();
    closure_with_snapshots(
        document,
        Some(importers),
        DocumentKind::Project,
        &roots,
        snapshots,
    )
}

fn closure_with_snapshots(
    document: &Value,
    importer_records: Option<&Value>,
    kind: DocumentKind,
    roots: &BTreeSet<String>,
    snapshot_roots: &BTreeSet<String>,
) -> Result<DependencyClosure, LockfileError> {
    let mut reached = DependencyClosure::default();
    let mut issues = Vec::new();
    let mut importers: VecDeque<_> = roots.iter().map(|key| (key.clone(), true)).collect();
    let mut queued_importers = roots.clone();
    let records: Vec<_> = section(document, "snapshots")
        .into_iter()
        .flatten()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    let mut traversal = Traversal {
        index: records
            .iter()
            .enumerate()
            .map(|(id, (key, _))| (*key, id))
            .collect(),
        visited: vec![false; records.len()],
        pending: Vec::new(),
    };
    for key in snapshot_roots {
        traversal.push("", key, kind, "snapshots", key, &mut issues);
    }
    while let Some((key, include_dev)) = importers.pop_front() {
        if !reached.importers.insert(key.clone()) {
            continue;
        }
        let Some(importer) = importer_records.and_then(|map| map.get(&key)) else {
            issues.push(issue(kind, &["importers", &key], "missing importer"));
            continue;
        };
        for (name, reference) in
            references(importer, kind, "importers", &key, include_dev, &mut issues)
        {
            if let Some(target) = reference.strip_prefix("link:") {
                let target = linked_importer(&key, target);
                if importer_records.and_then(|map| map.get(&target)).is_some()
                    && queued_importers.insert(target.clone())
                {
                    let include_dev = roots.contains(&target);
                    importers.push_back((target, include_dev));
                }
            } else {
                traversal.push(name, reference, kind, "importers", &key, &mut issues);
            }
        }
    }
    while let Some(id) = traversal.pending.pop() {
        let (key, snapshot) = records[id];
        let package = package_key(key);
        reached.packages.insert(package.to_owned());
        check_package(document, kind, key, &mut issues);
        for (name, reference) in references(snapshot, kind, "snapshots", key, false, &mut issues) {
            if !reference.starts_with("link:") {
                traversal.push(name, reference, kind, "snapshots", key, &mut issues);
            }
        }
    }
    reached.snapshots = records
        .iter()
        .zip(&traversal.visited)
        .filter(|(_, visited)| **visited)
        .map(|((key, _), _)| (*key).to_owned())
        .collect();
    if issues.is_empty() {
        Ok(reached)
    } else {
        Err(LockfileError::InvalidGraph(issues))
    }
}

/// Dense node IDs bound the pending work to V entries, even in cyclic or highly
/// shared graphs. Borrowed keys avoid copying peer suffixes into the work queue.
struct Traversal<'a> {
    index: HashMap<&'a str, usize>,
    visited: Vec<bool>,
    pending: Vec<usize>,
}

impl Traversal<'_> {
    fn push(
        &mut self,
        name: &str,
        reference: &str,
        kind: DocumentKind,
        section: &str,
        from: &str,
        issues: &mut Vec<ValidationIssue>,
    ) {
        let named = format!("{name}@{reference}");
        match self
            .index
            .get(named.as_str())
            .or_else(|| self.index.get(reference))
            .copied()
        {
            Some(id) if !self.visited[id] => {
                self.visited[id] = true;
                self.pending.push(id);
            }
            Some(_) => {}
            None => issues.push(issue(
                kind,
                &[section, from, name],
                format!("missing snapshot {named}"),
            )),
        }
    }
}

fn check_package(
    document: &Value,
    kind: DocumentKind,
    snapshot: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let key = package_key(snapshot);
    let package = document.get("packages").and_then(|map| map.get(key));
    match package {
        None => issues.push(issue(
            kind,
            &["snapshots", snapshot],
            format!("missing package {key}"),
        )),
        Some(package)
            if !package.is_object() || !package.get("resolution").is_some_and(Value::is_object) =>
        {
            issues.push(issue(
                kind,
                &["packages", key, "resolution"],
                "expected a resolution mapping",
            ));
        }
        _ => {}
    }
    if let Some(tail) = snapshot
        .strip_prefix(key)
        .and_then(|suffix| suffix.strip_prefix("(patch_hash="))
    {
        let hash = tail
            .split(')')
            .next()
            .expect("split always has a first element");
        let name = key
            .char_indices()
            .skip(1)
            .find(|(_, character)| *character == '@')
            .map_or(key, |(at, _)| &key[..at]);
        let declared = section(document, "patchedDependencies").is_some_and(|patches| {
            patches.iter().any(|(selector, patch)| {
                (selector == name
                    || selector
                        .strip_prefix(name)
                        .is_some_and(|suffix| suffix.starts_with('@')))
                    && patch
                        .as_str()
                        .or_else(|| patch.get("hash").and_then(Value::as_str))
                        == Some(hash)
            })
        });
        if !declared {
            issues.push(issue(
                kind,
                &["snapshots", snapshot],
                format!("patch hash {hash} is absent from patchedDependencies"),
            ));
        }
    }
}

fn check_catalogs(
    document: &Value,
    importer: &Value,
    kind: DocumentKind,
    key: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    for group in DEPENDENCIES {
        for (name, dependency) in importer
            .get(group)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let Some(catalog) = dependency
                .get("specifier")
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix("catalog:"))
            else {
                continue;
            };
            let catalog = if catalog.is_empty() {
                "default"
            } else {
                catalog
            };
            let entry = document
                .get("catalogs")
                .and_then(|v| v.get(catalog))
                .and_then(|v| v.get(name));
            let catalog_version = entry.and_then(|v| v.get("version")).and_then(Value::as_str);
            let actual_name = entry
                .and_then(|v| v.get("specifier"))
                .and_then(Value::as_str)
                .and_then(|specifier| specifier.strip_prefix("npm:"))
                .map(|alias| {
                    alias
                        .char_indices()
                        .skip(1)
                        .find(|(_, character)| *character == '@')
                        .map_or(alias, |(at, _)| &alias[..at])
                })
                .unwrap_or(name);
            let expected = catalog_version
                .map(|version| snapshot_key(actual_name, version, section(document, "snapshots")));
            let version = dependency
                .get("version")
                .and_then(Value::as_str)
                .map(|version| snapshot_key(name, version, section(document, "snapshots")));
            if expected.is_none()
                || version.as_deref().map(package_key) != expected.as_deref().map(package_key)
            {
                issues.push(issue(
                    kind,
                    &["importers", key, group, name],
                    format!("resolved version does not match catalogs/{catalog}/{name}"),
                ));
            }
        }
    }
}

fn check_overrides(
    overrides: &BTreeMap<&str, &str>,
    importer: &Value,
    kind: DocumentKind,
    key: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    for group in crate::manifest::GROUPS {
        for (name, dependency) in section(importer, group).into_iter().flatten() {
            if let Some(expected) = overrides.get(name.as_str())
                && dependency.get("specifier").and_then(Value::as_str) != Some(*expected)
            {
                issues.push(issue(
                    kind,
                    &["importers", key, group, name],
                    format!("specifier does not match overrides/{name}"),
                ));
            }
        }
    }
}

pub(crate) fn validate(document: &Value, kind: DocumentKind) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let overrides = crate::manifest::unconditional_overrides(document);
    for (key, importer) in section(document, "importers").into_iter().flatten() {
        check_references(document, importer, kind, "importers", key, &mut issues);
        check_catalogs(document, importer, kind, key, &mut issues);
        check_overrides(&overrides, importer, kind, key, &mut issues);
    }
    // Validation scans all edges once, including unreachable records. It does
    // not need a reachability walk or a per-edge diagnostic allocation on success.
    for (key, snapshot) in section(document, "snapshots").into_iter().flatten() {
        check_package(document, kind, key, &mut issues);
        check_references(document, snapshot, kind, "snapshots", key, &mut issues);
    }
    for key in keys(document, "packages") {
        check_package(document, kind, &key, &mut issues);
    }
    issues.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.message.cmp(&b.message)));
    issues.dedup();
    issues
}

fn check_references(
    document: &Value,
    record: &Value,
    kind: DocumentKind,
    source: &str,
    key: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let snapshots = section(document, "snapshots");
    for (name, reference) in references(record, kind, source, key, true, issues) {
        if reference.starts_with("link:") {
            continue;
        }
        let target = snapshot_key(name, reference, snapshots);
        if !snapshots.is_some_and(|map| map.contains_key(&target)) {
            issues.push(issue(
                kind,
                &[source, key, name],
                format!("missing snapshot {target}"),
            ));
        }
    }
}

pub(crate) fn prune(document: &mut Value, kind: DocumentKind) -> Result<(), LockfileError> {
    let reached = closure(document, kind, &keys(document, "importers"))?;
    retain(document, kind, reached)
}

/// An instance is optional only when every path from the importers crosses an
/// optional edge. Recompute usage after adding edges to the assembled graph.
pub(crate) fn refresh_optional(document: &mut Value) -> Result<(), LockfileError> {
    refresh_optional_in(document, None)
}

fn refresh_optional_in(
    document: &mut Value,
    live: Option<&BTreeSet<String>>,
) -> Result<(), LockfileError> {
    let Some(snapshots) = section(document, "snapshots") else {
        return Ok(());
    };
    let records: Vec<_> = snapshots.iter().collect();
    let mut traversal = Traversal {
        index: records
            .iter()
            .enumerate()
            .map(|(id, (key, _))| (key.as_str(), id))
            .collect(),
        visited: vec![false; records.len()],
        pending: Vec::new(),
    };
    let mut issues = Vec::new();
    for (id, importer) in section(document, "importers").into_iter().flatten() {
        for group in DEPENDENCIES
            .into_iter()
            .filter(|group| *group != "optionalDependencies")
        {
            for (name, dependency) in section(importer, group).into_iter().flatten() {
                let reference = dependency["version"]
                    .as_str()
                    .expect("validated importer reference");
                if !reference.starts_with("link:") {
                    traversal.push(
                        name,
                        reference,
                        DocumentKind::Project,
                        "importers",
                        id,
                        &mut issues,
                    );
                }
            }
        }
    }
    while let Some(id) = traversal.pending.pop() {
        let (key, record) = records[id];
        for (name, value) in section(record, "dependencies").into_iter().flatten() {
            let reference = value.as_str().expect("validated snapshot reference");
            if !reference.starts_with("link:") {
                traversal.push(
                    name,
                    reference,
                    DocumentKind::Project,
                    "snapshots",
                    key,
                    &mut issues,
                );
            }
        }
    }
    if !issues.is_empty() {
        return Err(LockfileError::InvalidGraph(issues));
    }
    let required = traversal.visited;
    for ((key, record), required) in document["snapshots"]
        .as_object_mut()
        .expect("snapshots")
        .iter_mut()
        .zip(required)
    {
        if live.is_some_and(|keys| !keys.contains(key)) {
            continue;
        }
        if required {
            record.as_object_mut().expect("snapshot").remove("optional");
        } else {
            record["optional"] = Value::Bool(true);
        }
    }
    Ok(())
}

/// Retaining old orphans also retains the records they reference. This avoids
/// unrelated cleanup while still removing records orphaned by the selected edit.
pub(crate) fn prune_scoped(
    document: &mut Value,
    kind: DocumentKind,
    baseline: &Value,
) -> Result<(), LockfileError> {
    let before = closure(baseline, kind, &keys(baseline, "importers"))?;
    let orphans = |field, reached: &BTreeSet<String>| -> BTreeSet<String> {
        section(baseline, field)
            .into_iter()
            .flat_map(|m| m.keys())
            .filter(|key| !reached.contains(*key))
            .cloned()
            .collect()
    };
    let mut old_snapshots = orphans("snapshots", &before.snapshots);
    let old_packages = orphans("packages", &before.packages);
    drop(before);
    let mut reached = closure(document, kind, &keys(document, "importers"))?;
    refresh_optional_in(document, Some(&reached.snapshots))?;
    old_snapshots.retain(|key| !reached.snapshots.contains(key));
    if !old_snapshots.is_empty() {
        let retained =
            closure_with_snapshots(document, None, kind, &BTreeSet::new(), &old_snapshots)?;
        reached.snapshots.extend(retained.snapshots);
        reached.packages.extend(retained.packages);
    }
    let mut issues = Vec::new();
    for key in &old_packages {
        check_package(document, kind, key, &mut issues);
    }
    if !issues.is_empty() {
        return Err(LockfileError::InvalidGraph(issues));
    }
    reached.packages.extend(old_packages);
    retain(document, kind, reached)
}

fn retain(
    document: &mut Value,
    kind: DocumentKind,
    reached: DependencyClosure,
) -> Result<(), LockfileError> {
    for (name, keep) in [
        ("snapshots", reached.snapshots),
        ("packages", reached.packages),
    ] {
        if let Some(map) = document.get_mut(name).and_then(Value::as_object_mut) {
            map.retain(|key, _| keep.contains(key));
        }
    }
    let mut issues = Vec::new();
    let overrides = crate::manifest::unconditional_overrides(document);
    for (key, importer) in section(document, "importers").into_iter().flatten() {
        check_catalogs(document, importer, kind, key, &mut issues);
        check_overrides(&overrides, importer, kind, key, &mut issues);
    }
    if !issues.is_empty() {
        return Err(LockfileError::InvalidGraph(issues));
    }
    Ok(())
}
