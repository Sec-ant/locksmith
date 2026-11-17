use std::{borrow::Cow, collections::BTreeMap};

use crate::{LockfileError, Value, graph};

pub(crate) const GROUPS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];
pub(crate) type Declarations<'a> = BTreeMap<&'a str, (&'static str, Cow<'a, str>)>;

/// pnpm precedence: optional > production > dev > auto-installed peers.
pub(crate) fn declarations<'a>(
    manifest: &'a Value,
    document: &'a Value,
    importer: &str,
) -> Result<Declarations<'a>, LockfileError> {
    let mut result = BTreeMap::new();
    let auto_peers = document["settings"]["autoInstallPeers"]
        .as_bool()
        .unwrap_or(true);
    for (field, group) in [
        ("peerDependencies", "dependencies"),
        ("devDependencies", "devDependencies"),
        ("dependencies", "dependencies"),
        ("optionalDependencies", "optionalDependencies"),
    ] {
        let Some(deps) = manifest.get(field) else {
            continue;
        };
        let deps = deps
            .as_object()
            .ok_or_else(|| LockfileError::InvalidManifest(importer.into()))?;
        for (name, specifier) in deps {
            let specifier = specifier
                .as_str()
                .ok_or_else(|| LockfileError::InvalidManifest(importer.into()))?;
            if let Some(specifier) = overridden(manifest, document, importer, name, specifier)?
                && (field != "peerDependencies"
                    || auto_peers
                    || specifier.parse::<node_semver::Range>().is_err())
            {
                result.insert(name.as_str(), (group, specifier));
            }
        }
    }
    if document["settings"]["excludeLinksFromLockfile"].as_bool() == Some(true) {
        result.retain(|_, (_, specifier)| !specifier.starts_with("link:"));
    }
    Ok(result)
}

pub(crate) fn mismatch(
    importer: &str,
    side: &'static str,
    message: impl Into<String>,
) -> LockfileError {
    LockfileError::ManifestMismatch {
        importer: importer.into(),
        side,
        message: message.into(),
    }
}

pub(crate) fn check(
    document: &Value,
    importer: &str,
    manifest: Option<&Value>,
    side: &'static str,
) -> Result<(), LockfileError> {
    let record = document.get("importers").and_then(|map| map.get(importer));
    let Some(manifest) = manifest else {
        return if record.is_none() {
            Ok(())
        } else {
            Err(mismatch(
                importer,
                side,
                "deleted/absent project still has an importer",
            ))
        };
    };
    if !manifest.is_object() {
        return Err(LockfileError::InvalidManifest(importer.into()));
    }
    let record = record
        .filter(|value| value.is_object())
        .ok_or_else(|| mismatch(importer, side, "missing importer"))?;
    let expected = declarations(manifest, document, importer)?;
    let mut actual = BTreeMap::new();
    for group in GROUPS {
        if let Some(deps) = record.get(group) {
            let deps = deps
                .as_object()
                .ok_or_else(|| mismatch(importer, side, format!("{group} must be a mapping")))?;
            for (name, dependency) in deps {
                let specifier = dependency
                    .get("specifier")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        mismatch(importer, side, format!("missing specifier for {name}"))
                    })?;
                let version = dependency
                    .get("version")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        mismatch(importer, side, format!("missing resolution for {name}"))
                    })?;
                if actual
                    .insert(name.as_str(), (group, Cow::Borrowed(specifier)))
                    .is_some()
                {
                    return Err(mismatch(
                        importer,
                        side,
                        format!("duplicate dependency {name}"),
                    ));
                }
                // npm ranges differ from Cargo's semver ranges. Non-registry
                // protocols (workspace, catalog, git, file) are graph-validated.
                let (range, version) = if let Some(alias) = specifier.strip_prefix("npm:") {
                    let (target, range) = alias
                        .rsplit_once('@')
                        .filter(|(name, _)| !name.is_empty())
                        .unwrap_or((alias, "*"));
                    let prefix = format!("{target}@");
                    let key =
                        graph::snapshot_key(name, version, graph::section(document, "snapshots"));
                    let resolved = graph::package_key(version)
                        .strip_prefix(&prefix)
                        .or_else(|| graph::package_key(&key).strip_prefix(&prefix))
                        .ok_or_else(|| {
                            mismatch(
                                importer,
                                side,
                                format!("alias {name} does not resolve to {target}"),
                            )
                        })?;
                    (range, Some(resolved.to_owned()))
                } else {
                    (specifier, Some(graph::package_key(version).to_owned()))
                };
                if let (Ok(range), Some(Ok(version))) = (
                    range.parse::<node_semver::Range>(),
                    version.map(|v| v.parse::<node_semver::Version>()),
                ) && !version.satisfies(&range)
                {
                    return Err(mismatch(
                        importer,
                        side,
                        format!("resolution of {name} does not satisfy {specifier}"),
                    ));
                }
            }
        }
    }
    if actual != expected {
        let name = expected
            .keys()
            .chain(actual.keys())
            .find(|name| expected.get(**name) != actual.get(**name))
            .expect("different maps have a differing key");
        return Err(mismatch(
            importer,
            side,
            format!(
                "dependency {name}: expected {:?}, found {:?}",
                expected.get(*name),
                actual.get(*name)
            ),
        ));
    }
    let empty = serde_json::json!({});
    if manifest.get("dependenciesMeta").unwrap_or(&empty)
        != record.get("dependenciesMeta").unwrap_or(&empty)
    {
        return Err(mismatch(importer, side, "dependenciesMeta differs"));
    }
    if manifest
        .get("publishConfig")
        .and_then(|v| v.get("directory"))
        != record.get("publishDirectory")
    {
        return Err(mismatch(importer, side, "publishDirectory differs"));
    }
    Ok(())
}

fn selector(value: &str) -> (&str, Option<&str>) {
    value
        .char_indices()
        .skip(1)
        .find(|(_, c)| *c == '@')
        .map_or((value, None), |(i, _)| (&value[..i], Some(&value[i + 1..])))
}
fn intersects(selector: Option<&str>, specifier: &str) -> bool {
    selector.is_none_or(|s|s==specifier || matches!((s.parse::<node_semver::Range>(),specifier.parse::<node_semver::Range>()),(Ok(a),Ok(b)) if a.allows_any(&b)))
}

/// Mirror pnpm's direct-dependency override selection. Parent-qualified rules
/// take precedence; overlapping rules use pnpm's most-specific selection order.
fn overridden<'a>(
    manifest: &Value,
    document: &'a Value,
    importer: &str,
    name: &str,
    specifier: &'a str,
) -> Result<Option<Cow<'a, str>>, LockfileError> {
    let mut generic: Option<(Option<&str>, &str)> = None;
    let mut qualified: Option<(Option<&str>, &str)> = None;
    for (rule, value) in graph::section(document, "overrides").into_iter().flatten() {
        let delimiter = rule
            .char_indices()
            .find(|(i, c)| *c == '>' && *i > 0 && !b" |@".contains(&rule.as_bytes()[i - 1]))
            .map(|(i, _)| i);
        let (parent, target) = delimiter.map_or((None, rule.as_str()), |i| {
            (Some(&rule[..i]), &rule[i + 1..])
        });
        if let Some(parent) = parent {
            let (parent_name, range) = selector(parent);
            if manifest.get("name").and_then(Value::as_str) != Some(parent_name) {
                continue;
            }
            if let Some(range) = range
                && !matches!((range.parse::<node_semver::Range>(),manifest.get("version").and_then(Value::as_str).and_then(|v|v.parse::<node_semver::Version>().ok())),(Ok(r),Some(v)) if v.satisfies(&r))
            {
                continue;
            }
        }
        let (target_name, range) = selector(target);
        if target_name != name || !intersects(range, specifier) {
            continue;
        }
        let value = value.as_str().ok_or_else(|| {
            LockfileError::InvalidDocument(format!("override {rule:?} must be a string"))
        })?;
        let best = if parent.is_some() {
            &mut qualified
        } else {
            &mut generic
        };
        if best.is_none_or(|(old, _)| intersects(old, range.unwrap_or(""))) {
            *best = Some((range, value));
        }
    }
    let Some((_, mut value)) = qualified.or(generic) else {
        return Ok(Some(Cow::Borrowed(specifier)));
    };
    if value == "-" {
        return Ok(None);
    }
    if let Some(catalog) = value.strip_prefix("catalog:") {
        value = document
            .get("catalogs")
            .and_then(|v| {
                v.get(if catalog.is_empty() {
                    "default"
                } else {
                    catalog
                })
            })
            .and_then(|v| v.get(name))
            .and_then(|v| v.get("specifier"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LockfileError::InvalidDocument(format!(
                    "override for {name} references a missing catalog entry"
                ))
            })?;
    }
    for protocol in ["link:", "file:"] {
        if let Some(path) = value.strip_prefix(protocol)
            && !path.starts_with('/')
            && path.as_bytes().get(1) != Some(&b':')
        {
            let path = graph::linked_importer(".", path);
            let from: Vec<_> = importer
                .split('/')
                .filter(|s| !s.is_empty() && *s != ".")
                .collect();
            let to: Vec<_> = path
                .split('/')
                .filter(|s| !s.is_empty() && *s != ".")
                .collect();
            let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
            let relative = std::iter::repeat_n("..", from.len() - common)
                .chain(to[common..].iter().copied())
                .collect::<Vec<_>>()
                .join("/");
            return Ok(Some(Cow::Owned(format!(
                "{protocol}{}",
                if relative.is_empty() { "." } else { &relative }
            ))));
        }
    }
    Ok(Some(Cow::Borrowed(value)))
}
