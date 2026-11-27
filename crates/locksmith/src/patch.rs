//! Interpret pnpm patch records, selection rules, and snapshot hash markers.
use crate::{LockfileError, Value, checksum, error::missing, pin, schema};
use serde_json::json;
use std::collections::HashMap;

pub(crate) fn hash_in_key(key: &str) -> Option<&str> {
    checked_hash_in_key(key).ok().flatten()
}

pub(crate) fn checked_hash_in_key(key: &str) -> Result<Option<&str>, &'static str> {
    let suffix = &key[pin::package_key(key).len()..];
    if !suffix.contains("(patch_hash=") {
        return Ok(None);
    }
    let mut depth = 0_usize;
    let mut hash = None;
    for (offset, byte) in suffix.bytes().enumerate() {
        match byte {
            b'(' => {
                if depth == 0
                    && let Some(rest) = suffix[offset..].strip_prefix("(patch_hash=")
                {
                    if offset != 0 {
                        return Err("snapshot patch hash must occur once before peer suffixes");
                    }
                    let (value, _) = rest
                        .split_once(')')
                        .filter(|(v, _)| !v.is_empty() && !v.contains('('))
                        .ok_or("expected a nonempty patch hash followed by ')'")?;
                    hash = Some(value);
                }
                depth += 1;
            }
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(hash)
}

// The outer None denotes competing selectors without a unique applicable patch.
pub(crate) fn desired_hash<'a>(
    key: &str,
    metadata: &'a Value,
    docs: [&Value; 4],
) -> Option<Option<&'a str>> {
    desired_hash_with_index(key, metadata, docs, None)
}

pub(crate) type PatchIndex<'a> = HashMap<&'a str, Vec<(&'a str, &'a Value)>>;

pub(crate) fn index(document: &Value) -> PatchIndex<'_> {
    let mut index = PatchIndex::new();
    for (selector, patch) in document
        .get("patchedDependencies")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let (name, range) = pin::selector(selector);
        if let Some(range) = range {
            index.entry(name).or_default().push((range, patch));
        }
    }
    index
}

pub(crate) fn desired_hash_with_index<'a>(
    key: &str,
    metadata: &'a Value,
    docs: [&Value; 4],
    index: Option<&PatchIndex<'a>>,
) -> Option<Option<&'a str>> {
    let package = pin::package_key(key);
    let Some(patches) = metadata
        .get("patchedDependencies")
        .and_then(Value::as_object)
    else {
        return Some(None);
    };
    if patches.is_empty() {
        return Some(None);
    }
    // Git, file and tarball locators identify the artifact. pnpm matches patch
    // selectors against its recorded release version, just as for a registry pin.
    let recorded;
    let package = if let Some(separator) = package
        .char_indices()
        .skip(1)
        .find_map(|(i, c)| (c == '@').then_some(i))
        && let (name, locator) = (&package[..separator], &package[separator + 1..])
        && locator.contains(':')
        && let Some(version) = docs
            .iter()
            .find_map(|doc| doc.get("packages")?.get(package)?.get("version")?.as_str())
    {
        recorded = format!("{name}@{version}");
        &recorded
    } else {
        package
    };
    let mut declared = patches.get(package);
    if declared.is_none()
        && let Some((name, version)) = package.rsplit_once('@')
    {
        let version = version.parse::<node_semver::Version>().ok();
        let matches = |(range, patch): (&str, &'a Value)| {
            let range = range.parse::<node_semver::Range>().ok()?;
            version
                .as_ref()
                .is_some_and(|v| v.satisfies(&range))
                .then_some((range, patch))
        };
        let matching: Vec<_> = match index {
            Some(index) => index
                .get(name)
                .into_iter()
                .flatten()
                .copied()
                .filter_map(matches)
                .collect(),
            None => patches
                .iter()
                .filter_map(|(selector, patch)| {
                    let range = selector.strip_prefix(name)?.strip_prefix('@')?;
                    matches((range, patch))
                })
                .collect(),
        };
        if matching.is_empty() {
            declared = patches.get(name);
        } else {
            let mut narrowest = matching
                .iter()
                .filter(|(range, _)| matching.iter().all(|(other, _)| other.allows_all(range)))
                .map(|(_, patch)| *patch);
            declared = narrowest.next();
            if declared.is_none() || narrowest.any(|p| Some(p) != declared) {
                return None;
            }
        }
    }
    let declared = declared.and_then(|v| v.as_str().or_else(|| v.get("hash")?.as_str()));
    Some(declared)
}

pub(crate) fn records(
    config: &crate::WorkspaceConfig,
    contents: &std::collections::BTreeMap<String, String>,
    encoding: crate::PatchEncoding,
) -> Result<Option<Value>, LockfileError> {
    let Some(patches) = config
        .patches
        .as_ref()
        .filter(|patches| !patches.is_empty())
    else {
        return Ok(None);
    };
    let records = patches
        .iter()
        .map(|(name, path)| {
            let contents = contents.get(path).ok_or_else(|| {
                missing(
                    &["patchedDependencies", name],
                    format!("missing patch contents: {path}"),
                )
            })?;
            let hash = checksum::patch_hash(contents, encoding);
            let record = match encoding {
                crate::PatchEncoding::Sha256 => Value::String(hash),
                crate::PatchEncoding::Sha256WithPath | crate::PatchEncoding::Md5Base32WithPath => {
                    json!({"hash": hash, "path": path})
                }
            };
            Ok((name.clone(), record))
        })
        .collect::<Result<_, LockfileError>>()?;
    Ok(Some(Value::Object(records)))
}

pub(crate) fn records_match(document: &Value, expected: Option<&Value>) -> bool {
    let recorded = schema::rule_map(document, "patchedDependencies");
    if recorded == expected {
        return true;
    }
    let (Some(recorded), Some(expected)) = (
        recorded.and_then(Value::as_object),
        expected.and_then(Value::as_object),
    ) else {
        return false;
    };
    recorded.len() == expected.len()
        && expected.iter().all(|(name, patch)| {
            let Some(old) = recorded.get(name) else {
                return false;
            };
            if old == patch {
                return true;
            }
            let paths = old["path"].as_str().zip(patch["path"].as_str());
            old.as_object().is_some_and(|entry| entry.len() == 2)
                && old["hash"] == patch["hash"]
                && paths.is_some_and(|(old, new)| {
                    use std::path::{Component, Path};
                    Path::new(old)
                        .components()
                        .filter(|c| *c != Component::CurDir)
                        .eq(Path::new(new)
                            .components()
                            .filter(|c| *c != Component::CurDir))
                })
        })
}
