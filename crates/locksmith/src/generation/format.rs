//! Version-specific representations written and read by pnpm.
mod read;
use super::Result;
use crate::{
    DocumentKind, Lockfile, LockfileError, Value, WorkspaceConfig, graph, manifest, peer,
    reconcile, source,
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

/// pnpm annotates immutable Git-hosted tarballs when importing older locks.
pub(super) fn mark_git_archives(doc: &mut Value, version: Option<&node_semver::Version>) {
    use node_semver::Version;
    let supported = version.is_some_and(|v| match v.major() {
        10 => v >= &Version::from((10, 33, 4)),
        11 => v >= &Version::from((11, 1, 0)),
        major => major >= 12,
    });
    if !supported {
        return;
    }
    for record in doc
        .get_mut("packages")
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|m| m.values_mut())
    {
        let resolution = &mut record["resolution"];
        if resolution.get("type").is_none()
            && resolution.get("gitHosted").is_none()
            && resolution["tarball"]
                .as_str()
                .is_some_and(immutable_git_archive)
        {
            resolution["gitHosted"] = true.into();
        }
    }
}

fn immutable_git_archive(tarball: &str) -> bool {
    let Ok(url) = tarball.parse::<http::Uri>() else {
        return false;
    };
    if url.scheme() != Some(&http::uri::Scheme::HTTPS) {
        return false;
    }
    let Some(host) = url.host() else { return false };
    let path: Vec<_> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let commit = |s: &str| s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit());
    match (host.to_ascii_lowercase().as_str(), path.as_slice()) {
        ("codeload.github.com", [_, _, "tar.gz", id]) => commit(id),
        ("bitbucket.org", [_, _, "get", file]) => file.strip_suffix(".tar.gz").is_some_and(commit),
        ("gitlab.com", ["api", "v4", "projects", _, "repository", "archive.tar.gz"]) => url
            .query()
            .into_iter()
            .flat_map(|s| s.split('&'))
            .find_map(|s| s.strip_prefix("ref="))
            .is_some_and(commit),
        ("gitlab.com", [_, _, .., "-", "archive", id, file]) => {
            commit(id) && file.ends_with(".tar.gz")
        }
        _ => false,
    }
}

const SETTINGS: [&str; 6] = [
    "catalogs",
    "overrides",
    "packageExtensionsChecksum",
    "pnpmfileChecksum",
    "ignoredOptionalDependencies",
    "patchedDependencies",
];

impl Lockfile {
    pub fn parse_generation_input(text: String) -> Result<Self> {
        // pnpm drops a byte order mark before it detects the document format.
        // The v9 parser keeps the original bytes.
        let view = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if let Some(first) = view
            .lines()
            .next()
            .and_then(|s| s.strip_prefix("lockfileVersion:"))
            && let Ok(version) = serde_saphyr::from_str::<Value>(first.trim())
            && (version.is_number() || version.is_string())
            && !matches!(version.as_str(), Some("9.0" | "13.0"))
            && version.as_f64() != Some(9.0)
            && version.as_f64() != Some(13.0)
        {
            return Err(LockfileError::UnsupportedVersion { version });
        }
        let ranges = source::document_ranges(view);
        let Some(last) = ranges.last() else {
            return Self::try_from(text);
        };
        let part = &view[last.clone()];
        let first = part.trim_start();
        let json = first.starts_with('{');
        if !json
            && !part.lines().any(|l| {
                l.trim_start_matches(['"', '\''])
                    .starts_with("lockfileVersion")
                    && l.split_once(':')
                        .is_some_and(|(_, v)| v.trim_matches([' ', '\'', '"']).starts_with("13.0"))
            })
        {
            return Self::try_from(text);
        }
        let next: Value = if json {
            json5::from_str(part).map_err(|e| LockfileError::InvalidDocument(e.to_string()))?
        } else {
            serde_saphyr::from_str(part)?
        };
        if next["lockfileVersion"] != "13.0" {
            return Self::try_from(text);
        }
        let project = read::restore(&next)?;
        if ranges.len() > 2 {
            return Err(LockfileError::InvalidDocument(
                "multiple documents before v13 project".into(),
            ));
        }
        let mut result = if ranges.len() == 2 {
            let env = Self::parse(&format!("---\n{}", &view[ranges[0].clone()]))?;
            if env.environment().is_none() {
                return Err(LockfileError::InvalidDocument(
                    "expected bootstrap document before v13 project".into(),
                ));
            }
            env
        } else {
            Self::with_documents(BTreeMap::new(), &[])
        };
        result
            .documents
            .insert(DocumentKind::Project, Arc::new(project));
        result.validate()?;
        Ok(result)
    }

    /// Serialize the v13 project in pnpm's JSON-compatible wire format. The
    /// bootstrap graph remains an independent v9 YAML document.
    pub fn to_pnpm_v13(&self) -> Result<String> {
        let doc = self
            .project()
            .ok_or_else(|| LockfileError::InvalidDocument("missing project document".into()))?;
        let next = encode(doc)?;
        if read::restore(&next)? != *doc {
            return Err(LockfileError::OutputMismatch);
        }
        let mut out = String::new();
        if let Some(env) = self.documents.get(&DocumentKind::Environment) {
            let lock = Self::with_documents(
                BTreeMap::from([(DocumentKind::Environment, env.clone())]),
                &[self],
            );
            out = lock.to_yaml()?;
            out.push_str("\n---\n");
        }
        out.push_str(&serde_json::to_string_pretty(&next).expect("JSON value"));
        out.push('\n');
        Ok(out)
    }
}

fn encode(doc: &Value) -> Result<Value> {
    let bindings = peer::snapshot_bindings(doc)?;
    let mut variants = BTreeMap::<&str, usize>::new();
    for key in bindings.keys() {
        *variants.entry(graph::package_key(key)).or_default() += 1;
    }
    let wire = Wire {
        doc,
        bindings: &bindings,
        variants,
    };
    let mut out = doc.clone();
    let map = out.as_object_mut().expect("project mapping");
    for f in ["importers", "packages", "snapshots"] {
        map.remove(f);
    }
    out["lockfileVersion"] = "13.0".into();
    for f in SETTINGS {
        if let Some(value) = out.as_object_mut().unwrap().remove(f) {
            out["settings"][f] = value;
        }
    }
    for (id, importer) in graph::section(doc, "importers").into_iter().flatten() {
        let mut project = importer.clone();
        for group in manifest::GROUPS {
            for (name, dep) in graph::section(importer, group).into_iter().flatten() {
                let pin = dep["version"].as_str().expect("validated importer");
                let value = wire.reference(name, pin, &mut Vec::new());
                let mut entry = dep.clone();
                entry["version"] = raw_ref(&value).into();
                if let Some(peers) = value.get("with") {
                    entry["with"] = peers.clone();
                }
                if entry["version"] == dep["specifier"] && entry.get("with").is_none() {
                    entry = dep["specifier"].clone();
                }
                project[group][name] = entry;
            }
        }
        out["projects"][id] = project;
    }
    for (base, raw) in graph::section(doc, "packages").into_iter().flatten() {
        let mut record = raw.clone();
        if let Some(hash) = record["resolution"]
            .as_object_mut()
            .and_then(|m| m.remove("integrity"))
        {
            record["integrity"] = hash;
        }
        if record["resolution"]
            .as_object()
            .is_some_and(|m| m.is_empty())
        {
            record.as_object_mut().unwrap().remove("resolution");
        }
        record
            .as_object_mut()
            .unwrap()
            .remove("peerDependenciesMeta");
        for (name, meta) in graph::section(raw, "peerDependenciesMeta")
            .into_iter()
            .flatten()
        {
            let mut entry = meta.clone();
            if let Some(spec) = raw["peerDependencies"].get(name) {
                entry["specifier"] = spec.clone();
            }
            record["peerDependencies"][name] = entry;
        }
        out["packages"][base] = record;
    }
    for (key, body) in graph::section(doc, "snapshots").into_iter().flatten() {
        let base = graph::package_key(key);
        let package = &doc["packages"][base];
        // A declared peer the instance does not bind can still be an ordinary
        // dependency. Only an explicit peer suffix shows which names are bound.
        let bound = crate::peer::explicit_bindings(key);
        let mut snapshot = body.clone();
        for group in ["dependencies", "optionalDependencies"] {
            if let Some(children) = snapshot.get_mut(group).and_then(Value::as_object_mut) {
                children.retain(|name, _| {
                    package["peerDependencies"].get(name).is_none()
                        || bound
                            .as_ref()
                            .is_some_and(|b| !b.contains_key(name.as_str()))
                });
                for (name, value) in children.iter_mut() {
                    *value = wire.reference(
                        name,
                        value.as_str().expect("validated reference"),
                        &mut Vec::new(),
                    );
                }
                if children.is_empty() {
                    snapshot.as_object_mut().unwrap().remove(group);
                }
            }
        }
        snapshot
            .as_object_mut()
            .unwrap()
            .remove("transitivePeerDependencies");
        let bound = &bindings[key];
        for (name, pin) in bound
            .iter()
            .filter(|(name, _)| name.as_str() != "patch_hash")
        {
            let pin = graph::dependency(body, name).map_or(pin.as_str(), |(_, v)| v);
            let value = wire.reference(name, pin, &mut vec![key.to_owned()]);
            let mut entry = json!({"version":raw_ref(&value)});
            if let Some(peers) = value.get("with") {
                entry["with"] = peers.clone();
            }
            if package["peerDependencies"].get(name).is_none() {
                entry["below"] = true.into();
            }
            snapshot["peerDependencies"][name] = entry;
        }
        let unbound: Vec<_> = graph::transitive(body)
            .filter(|n| !bound.contains_key(*n))
            .collect();
        if !unbound.is_empty() {
            snapshot["unboundPeerDependencies"] = json!(unbound);
        }
        let record = &mut out["packages"][base];
        if wire.variants[base] > 1 {
            record
                .as_object_mut()
                .unwrap()
                .entry("snapshots")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap()
                .push(snapshot);
        } else {
            for (field, v) in snapshot.as_object().unwrap() {
                if field == "peerDependencies" {
                    for (name, b) in v.as_object().unwrap() {
                        let old = record[field].get(name);
                        let mut entry = match old {
                            Some(Value::String(s)) => json!({"specifier":s}),
                            Some(v) => v.clone(),
                            None => json!({}),
                        };
                        entry
                            .as_object_mut()
                            .unwrap()
                            .extend(b.as_object().unwrap().clone());
                        record[field][name] = entry;
                    }
                } else {
                    record[field] = v.clone();
                }
            }
        }
    }
    Ok(out)
}

struct Wire<'a> {
    doc: &'a Value,
    bindings: &'a BTreeMap<String, BTreeMap<String, String>>,
    variants: BTreeMap<&'a str, usize>,
}
impl Wire<'_> {
    fn reference(&self, name: &str, pin: &str, active: &mut Vec<String>) -> Value {
        let version = graph::package_key(pin);
        let key = graph::snapshot_key(name, pin, graph::section(self.doc, "snapshots"));
        let Some(bindings) = self.bindings.get(&key) else {
            return version.into();
        };
        if self.variants[graph::package_key(&key)] == 1 || active.contains(&key) {
            return version.into();
        }
        active.push(key.clone());
        let bound: serde_json::Map<_, _> = bindings
            .iter()
            .filter(|(n, _)| n.as_str() != "patch_hash")
            .map(|(n, p)| {
                let pin = graph::dependency(&self.doc["snapshots"][&key], n)
                    .map_or(p.as_str(), |(_, v)| v);
                (n.clone(), self.reference(n, pin, active))
            })
            .collect();
        active.pop();
        json!({"version":version,"with":bound})
    }
}
fn raw_ref(value: &Value) -> &str {
    value
        .as_str()
        .or_else(|| value["version"].as_str())
        .unwrap_or_default()
}

fn policy(doc: &Value) -> Result<WorkspaceConfig> {
    WorkspaceConfig::from_pnpm_version(&doc["settings"], &node_semver::Version::from((12, 21, 0)))
}
