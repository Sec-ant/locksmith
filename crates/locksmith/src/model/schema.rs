//! Structural checks for lockfile records shared by parsing and graph validation.
use std::{collections::BTreeMap, fmt};

use serde_json::Map;

use crate::{LockfileError, ValidationIssue, Value, model::error::issue, model::pin};

/// The project dependency graph or pnpm's package-manager bootstrap graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DocumentKind {
    /// Bootstrap dependencies for the package manager itself.
    Environment,
    /// Workspace importers and their package graph.
    Project,
}

impl fmt::Display for DocumentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Environment => "environment",
            Self::Project => "project",
        })
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

pub(crate) fn check_package_peers(
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

pub(crate) const DEPENDENCY_GROUPS: [&str; 3] =
    ["dependencies", "devDependencies", "optionalDependencies"];

/// pnpm omits rule maps with no entries when writing the lockfile.
pub(crate) fn rule_map<'a>(document: &'a Value, field: &str) -> Option<&'a Value> {
    document
        .get(field)
        .filter(|value| !value.as_object().is_some_and(|rules| rules.is_empty()))
}

pub(crate) fn same_override_order(left: &Value, right: &Value) -> bool {
    same_ordered_mapping(left.get("overrides"), right.get("overrides"))
}

pub(crate) fn same_ordered_mapping(left: Option<&Value>, right: Option<&Value>) -> bool {
    left == right
        && left
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .eq(right.and_then(Value::as_object).into_iter().flatten())
}

pub(crate) fn section<'a>(document: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    document.get(key).and_then(Value::as_object)
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

pub(crate) struct PnpmManager<'a> {
    pub specifier: &'a str,
    pub requires_pin: bool,
    pub(crate) on_fail: Option<&'a str>,
}

impl<'a> PnpmManager<'a> {
    pub fn declared(manifest: &'a Value, runner: Option<&node_semver::Version>) -> Option<Self> {
        let legacy = legacy_pnpm(manifest);
        // pnpm 9 and 10 ignore devEngines, including npm's own engine declaration.
        let runner = runner.or_else(|| legacy.as_ref().map(|(_, version)| version));
        let engines = &manifest["devEngines"]["packageManager"];
        let (engine, default_policy) = match engines.as_array() {
            Some(engines) => {
                let index = engines
                    .iter()
                    .position(|e| e["name"] == "pnpm")
                    .unwrap_or(0);
                (
                    engines.get(index),
                    (index + 1 < engines.len()).then_some("ignore"),
                )
            }
            None => (Some(engines), None),
        };
        if runner.is_none_or(|v| v.major() >= 11)
            && let Some(engine) =
                engine.filter(|e| e["name"].as_str().is_some_and(|s| !s.is_empty()))
        {
            let specifier = engine["version"].as_str()?;
            if engine["name"] != "pnpm" || specifier.parse::<node_semver::Range>().is_err() {
                return None;
            }
            return Some(Self {
                specifier,
                requires_pin: true,
                on_fail: engine["onFail"].as_str().or(default_policy),
            });
        }
        let (specifier, version) = legacy?;
        Some(Self {
            specifier,
            requires_pin: version.major() >= 12,
            on_fail: None,
        })
    }
}

fn legacy_pnpm(manifest: &Value) -> Option<(&str, node_semver::Version)> {
    let declared = manifest["packageManager"].as_str()?.strip_prefix("pnpm@")?;
    let specifier = declared.split('+').next()?;
    let version = specifier.parse::<node_semver::Version>().ok()?;
    (version.to_string() == specifier).then_some((specifier, version))
}

pub(crate) fn override_selector(rule: &str) -> (Option<&str>, &str, Option<&str>) {
    let delimiter = rule
        .char_indices()
        .find(|(i, c)| *c == '>' && *i > 0 && !b" |@".contains(&rule.as_bytes()[i - 1]))
        .map(|(i, _)| i);
    let (parent, target) = delimiter.map_or((None, rule), |i| (Some(&rule[..i]), &rule[i + 1..]));
    let (name, range) = pin::selector(target);
    (parent, name, range)
}

pub(crate) fn override_scope(rule: &str) -> (&str, Option<&str>) {
    let (parent, name, _) = override_selector(rule);
    (name, parent.map(|parent| pin::selector(parent).0))
}

pub(crate) fn parent_matches(selector: &str, name: &str, version: &str) -> bool {
    selector == name
        || selector
            .strip_prefix(name)
            .and_then(|s| s.strip_prefix('@'))
            .is_some_and(|r| {
                matches!(
                    (r.parse::<node_semver::Range>(), version.parse::<node_semver::Version>()),
                    (Ok(range), Ok(version)) if version.satisfies(&range)
                )
            })
}

/// Registry overrides whose effective direct specifier is known without a
/// manifest. Qualified rules require the original declaration or parent identity,
/// neither of which is fully represented in an importer.
pub(crate) fn unconditional_overrides(document: &Value) -> BTreeMap<&str, &str> {
    let mut candidates = BTreeMap::new();
    let rules = section(document, "overrides");
    for (rule, value) in rules.into_iter().flatten() {
        let (parent, name, range) = override_selector(rule);
        if parent.is_none()
            && range.is_none()
            && let Some(value) = value.as_str()
            && value.parse::<node_semver::Range>().is_ok()
        {
            candidates.insert(name, value);
        }
    }
    for (rule, _) in rules.into_iter().flatten() {
        let (parent, name, range) = override_selector(rule);
        if parent.is_some() || range.is_some() {
            candidates.remove(name);
        }
    }
    candidates
}
