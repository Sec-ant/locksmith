//! Structural checks for lockfile records shared by parsing and graph validation.
use serde_json::Map;

use crate::{DocumentKind, LockfileError, ValidationIssue, Value, error::issue};

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
