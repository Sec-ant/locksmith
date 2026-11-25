use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

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

pub(crate) fn dependency<'a>(body: &'a Value, name: &str) -> Option<(&'static str, &'a str)> {
    ["dependencies", "optionalDependencies"]
        .into_iter()
        .find_map(|group| Some((group, body[group].get(name)?.as_str()?)))
}

pub(crate) fn transitive(body: &Value) -> impl Iterator<Item = &str> {
    body["transitivePeerDependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
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

/// A file pin is relative to the lockfile root. Aliases record the package name too.
pub(crate) fn file_pin<'a>(name: &'a str, reference: &'a str) -> Option<(&'a str, &'a str)> {
    let pin = package_key(reference);
    pin.strip_prefix("file:")
        .map(|path| (name, path))
        .or_else(|| pin.split_once("@file:"))
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
) -> Vec<(&'static str, &'a str, &'a str)> {
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
                Some(reference) if !reference.is_empty() => {
                    result.push((group, name.as_str(), reference))
                }
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

/// A workspace link can point at the project's publish directory.
pub(crate) fn linked_project(importers: Option<&Value>, from: &str, path: &str) -> String {
    let target = linked_importer(from, path);
    let Some(importers) = importers.and_then(Value::as_object) else {
        return target;
    };
    if importers.contains_key(&target) {
        return target;
    }
    let publishes_to_target = |id: &str, record: &Value| {
        record["linkDirectory"] != false
            && record["publishDirectory"]
                .as_str()
                .is_some_and(|dir| linked_importer(id, dir) == target)
    };
    let mut ancestor = target.as_str();
    while ancestor != "." {
        ancestor = ancestor.rsplit_once('/').map_or(".", |(parent, _)| parent);
        if let Some(record) = importers.get(ancestor)
            && publishes_to_target(ancestor, record)
        {
            return ancestor.to_owned();
        }
    }
    let mut owners = importers
        .iter()
        .filter(|(id, record)| publishes_to_target(id, record));
    if let Some((owner, _)) = owners.next()
        && owners.next().is_none()
    {
        return owner.clone();
    }
    target
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
    let mut importers: VecDeque<_> = roots.iter().cloned().collect();
    let mut queued_importers = roots.clone();
    let mut traversal = Traversal {
        snapshots: section(document, "snapshots"),
        ..Traversal::default()
    };
    for key in snapshot_roots {
        traversal.push("", key, kind, "snapshots", key, &mut issues);
    }
    loop {
        let importer_id = importers.pop_front();
        let (source, key, record, include_dev, link_base) = if let Some(key) = &importer_id {
            reached.importers.insert(key.clone());
            let Some(record) = importer_records.and_then(|map| map.get(key)) else {
                issues.push(issue(kind, &["importers", key], "missing importer"));
                continue;
            };
            (
                "importers",
                key.as_str(),
                record,
                roots.contains(key),
                key.as_str(),
            )
        } else if let Some((key, snapshot)) = traversal.pending.pop() {
            reached.packages.insert(package_key(key).to_owned());
            check_package(document, kind, key, &mut issues);
            ("snapshots", key, snapshot, false, ".")
        } else {
            break;
        };
        for (_, name, reference) in references(record, kind, source, key, include_dev, &mut issues)
        {
            if let Some(target) = reference.strip_prefix("link:") {
                let target = linked_project(importer_records, link_base, target);
                if importer_records.and_then(|map| map.get(&target)).is_some()
                    && queued_importers.insert(target.clone())
                {
                    importers.push_back(target);
                }
            } else {
                traversal.push(name, reference, kind, source, key, &mut issues);
            }
        }
    }
    reached.snapshots = traversal.visited.into_iter().map(str::to_owned).collect();
    if issues.is_empty() {
        Ok(reached)
    } else {
        Err(LockfileError::InvalidGraph(issues))
    }
}

/// Use the lock's existing key index. Only reachable entries need traversal state.
#[derive(Default)]
struct Traversal<'a> {
    snapshots: Option<&'a Map<String, Value>>,
    visited: HashSet<&'a str>,
    pending: Vec<(&'a str, &'a Value)>,
    named: String,
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
        self.named.clear();
        self.named.push_str(name);
        self.named.push('@');
        self.named.push_str(reference);
        match self.snapshots.and_then(|entries| {
            entries
                .get_key_value(&self.named)
                .or_else(|| entries.get_key_value(reference))
        }) {
            Some((key, record)) if self.visited.insert(key) => {
                self.pending.push((key, record));
            }
            Some(_) => {}
            None => issues.push(issue(
                kind,
                &[section, from, name],
                format!("missing snapshot {}", self.named),
            )),
        }
    }
}

/// Peer declarations are input data. Silently skipping an ill-shaped entry
/// would turn a corrupt declaration into a successful empty inspection.
pub(crate) fn check_peer_declarations(
    kind: DocumentKind,
    packages: &Map<String, Value>,
) -> Result<(), LockfileError> {
    let mut issues = Vec::new();
    for (key, package) in packages {
        check_package_peers(kind, key, package, &mut issues);
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(LockfileError::InvalidGraph(issues))
    }
}

fn check_package_peers(
    kind: DocumentKind,
    key: &str,
    package: &Value,
    issues: &mut Vec<ValidationIssue>,
) {
    for field in ["peerDependencies", "peerDependenciesMeta"] {
        let Some(value) = package.get(field) else {
            continue;
        };
        let Some(entries) = value.as_object() else {
            issues.push(issue(kind, &["packages", key, field], "expected a mapping"));
            continue;
        };
        for (name, value) in entries {
            if field == "peerDependencies" && !value.is_string() {
                issues.push(issue(
                    kind,
                    &["packages", key, field, name],
                    "expected a version range string",
                ));
            } else if field == "peerDependenciesMeta" {
                if !value.is_object() {
                    issues.push(issue(
                        kind,
                        &["packages", key, field, name],
                        "expected a mapping",
                    ));
                } else if value.get("optional").is_some_and(|v| !v.is_boolean()) {
                    issues.push(issue(
                        kind,
                        &["packages", key, field, name, "optional"],
                        "expected a boolean",
                    ));
                }
            }
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
    config: Option<&crate::WorkspaceConfig>,
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
            let spec = match config {
                Some(c) => c
                    .catalogs
                    .get(catalog)
                    .and_then(|m| m.get(name))
                    .map(String::as_str),
                None => entry.and_then(|v| v["specifier"].as_str()),
            };
            let pin = dependency["version"].as_str();
            let fits = if let Some(path) = pin.and_then(|v| v.strip_prefix("link:")) {
                let target = linked_project(document.get("importers"), key, path);
                document["importers"].get(&target).is_some()
                    && match (spec, entry) {
                        (Some(spec), Some(entry)) => {
                            crate::reconcile::metadata::catalog_link_entry_fits(spec, entry)
                        }
                        (Some(_), None) => true,
                        // pnpm 12.0–12.5 omitted catalog records used only by local links.
                        (None, None) => config.is_none(),
                        (None, Some(_)) => false,
                    }
            } else {
                spec.zip(pin).is_some_and(|(spec, pin)| {
                    crate::reconcile::metadata::catalog_consumer_fits(name, spec, pin, entry)
                })
            };
            if !fits {
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
    validate_with_config(document, kind, None)
}

pub(crate) fn validate_with_config(
    document: &Value,
    kind: DocumentKind,
    config: Option<&crate::WorkspaceConfig>,
) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let overrides = crate::manifest::unconditional_overrides(document);
    for (key, importer) in section(document, "importers").into_iter().flatten() {
        check_references(document, importer, kind, "importers", key, &mut issues);
        check_catalogs(document, importer, kind, key, config, &mut issues);
        check_overrides(&overrides, importer, kind, key, &mut issues);
    }
    // Validation scans all edges once, including unreachable records. It does
    // not need a reachability walk or a per-edge diagnostic allocation on success.
    for (key, snapshot) in section(document, "snapshots").into_iter().flatten() {
        check_package(document, kind, key, &mut issues);
        check_references(document, snapshot, kind, "snapshots", key, &mut issues);
    }
    for name in ["importers", "snapshots", "packages"] {
        if document.get(name).is_some_and(|value| !value.is_object()) {
            issues.push(issue(kind, &[name], "expected a mapping"));
        }
    }
    for (key, package) in section(document, "packages").into_iter().flatten() {
        check_package(document, kind, key, &mut issues);
        check_package_peers(kind, key, package, &mut issues);
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
    for (_, name, reference) in references(record, kind, source, key, true, issues) {
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
    retain(document, kind, reached, None)
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
    let mut traversal = Traversal {
        snapshots: Some(snapshots),
        ..Traversal::default()
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
    while let Some((key, record)) = traversal.pending.pop() {
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
    let required: Vec<_> = snapshots
        .keys()
        .map(|key| traversal.visited.contains(key.as_str()))
        .collect();
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
    config: Option<&crate::WorkspaceConfig>,
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
    retain(document, kind, reached, config)
}

fn retain(
    document: &mut Value,
    kind: DocumentKind,
    reached: DependencyClosure,
    config: Option<&crate::WorkspaceConfig>,
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
        check_catalogs(document, importer, kind, key, config, &mut issues);
        check_overrides(&overrides, importer, kind, key, &mut issues);
    }
    if !issues.is_empty() {
        return Err(LockfileError::InvalidGraph(issues));
    }
    Ok(())
}
