use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

use super::{Result, SETTINGS, Value, graph, manifest, policy, raw_ref, reconcile};

struct Variant {
    base: String,
    body: Value,
    bound: BTreeMap<String, Value>,
    scope: BTreeMap<String, Value>,
    below: BTreeSet<String>,
}

fn object(v: &Value) -> Result<&serde_json::Map<String, Value>> {
    v.as_object()
        .ok_or_else(|| reconcile::missing(&["lockfileVersion", "13.0"], "expected a mapping"))
}
fn combined(shared: &Value, extra: &Value, field: &str) -> Result<BTreeMap<String, Value>> {
    let mut merged = BTreeMap::new();
    for source in [shared, extra] {
        if let Some(value) = source.get(field) {
            merged.extend(object(value)?.clone());
        }
    }
    Ok(merged)
}
fn choose(
    name: &str,
    reference: &Value,
    scope: &BTreeMap<String, Value>,
    groups: &BTreeMap<String, Vec<usize>>,
    variants: &[Variant],
) -> Option<usize> {
    let version = raw_ref(reference);
    let named = format!("{name}@{version}");
    let ids = groups.get(&named).or_else(|| groups.get(version))?;
    let wanted = reference["with"].as_object();
    if let Some(wanted) = wanted
        && let Some(&id) = ids
            .iter()
            .filter(|&&id| {
                wanted
                    .iter()
                    .all(|(name, v)| variants[id].bound.get(name).is_some_and(|b| names(v, b)))
            })
            .min_by_key(|&&id| variants[id].bound.len())
    {
        return Some(id);
    }
    ids.iter()
        .filter_map(|&id| {
            let mut agrees = 0;
            let mut absent = 0;
            for (name, b) in &variants[id].bound {
                match scope.get(name) {
                    Some(s) if raw_ref(b) == raw_ref(s) => agrees += 1,
                    Some(_) => return None,
                    None => absent += 1,
                }
            }
            Some(((std::cmp::Reverse(agrees), absent, id), id))
        })
        .min()
        .map(|(_, id)| id)
        .or_else(|| ids.first().copied())
}
fn names(wanted: &Value, bound: &Value) -> bool {
    raw_ref(wanted) == raw_ref(bound)
        && wanted["with"]
            .as_object()
            .into_iter()
            .flatten()
            .all(|(n, v)| bound["with"].get(n).is_some_and(|b| names(v, b)))
}

pub(super) fn restore(next: &Value) -> Result<Value> {
    let mut doc = next.clone();
    let map = doc
        .as_object_mut()
        .ok_or_else(|| reconcile::missing(&[], "expected a project mapping"))?;
    for field in ["packages", "projects"] {
        if map
            .remove(field)
            .is_some_and(|section| !section.is_object())
        {
            return Err(reconcile::missing(&[field], "expected a mapping"));
        }
    }
    doc["lockfileVersion"] = "9.0".into();
    for field in SETTINGS {
        if let Some(v) = doc
            .get_mut("settings")
            .and_then(Value::as_object_mut)
            .and_then(|s| s.remove(field))
        {
            doc[field] = v;
        }
    }
    if let Some(catalogs) = doc.get_mut("catalogs").and_then(Value::as_object_mut) {
        for entries in catalogs.values_mut() {
            for entry in entries
                .as_object_mut()
                .ok_or_else(|| reconcile::missing(&["catalogs"], "expected entries"))?
                .values_mut()
            {
                if let Some(spec) = entry.as_str() {
                    let version = reconcile::alias(spec).map_or(spec, |(_, s)| s);
                    *entry = json!({"specifier":spec,"version":version});
                }
            }
        }
    }
    let mut groups = BTreeMap::new();
    let mut variants = Vec::new();
    for (base, record) in graph::section(next, "packages").into_iter().flatten() {
        let mut raw = record.clone();
        let fields = raw
            .as_object_mut()
            .ok_or_else(|| reconcile::missing(&["packages", base], "expected a mapping"))?;
        for field in [
            "snapshots",
            "integrity",
            "dependencies",
            "optionalDependencies",
            "peerDependencies",
            "id",
            "optional",
            "unboundPeerDependencies",
        ] {
            fields.remove(field);
        }
        if let Some(hash) = record.get("integrity") {
            if raw.get("resolution").is_some_and(|r| !r.is_object()) {
                return Err(reconcile::missing(
                    &["packages", base, "resolution"],
                    "expected a mapping",
                ));
            }
            raw["resolution"]["integrity"] = hash.clone();
        }
        if raw.get("resolution").is_none()
            && let Some(path) = manifest::selector(base)
                .1
                .and_then(|s| s.strip_prefix("file:"))
        {
            raw["resolution"] = json!({"type":"directory","directory":path});
        }
        for (name, entry) in graph::section(record, "peerDependencies")
            .into_iter()
            .flatten()
        {
            if let Some(spec) = entry.as_str().or_else(|| entry["specifier"].as_str()) {
                raw["peerDependencies"][name] = spec.into();
            }
            if let Some(optional) = entry.get("optional") {
                raw["peerDependenciesMeta"][name] = json!({"optional":optional});
            }
        }
        doc["packages"][base] = raw;
        let extras = record["snapshots"]
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or(&[Value::Null]);
        for extra in extras {
            let mut body = json!({});
            for field in ["id", "optional", "unboundPeerDependencies"] {
                if let Some(v) = extra.get(field).or_else(|| record.get(field)) {
                    body[field] = v.clone();
                }
            }
            let mut scope = BTreeMap::new();
            scope.insert(
                manifest::selector(base).0.into(),
                reconcile::reference(manifest::selector(base).0, base).into(),
            );
            for field in ["dependencies", "optionalDependencies"] {
                let deps = combined(record, extra, field)?;
                if !deps.is_empty() {
                    body[field] = json!(deps);
                }
                scope.extend(deps);
            }
            let mut bound = BTreeMap::new();
            let mut below = BTreeSet::new();
            for (name, entry) in combined(record, extra, "peerDependencies")? {
                if let Some(version) = entry.get("version") {
                    let value = if let Some(p) = entry.get("with") {
                        json!({"version":version,"with":p})
                    } else {
                        version.clone()
                    };
                    scope.insert(name.clone(), value.clone());
                    if entry["below"] == true {
                        below.insert(name.clone());
                    }
                    bound.insert(name, value);
                }
            }
            groups
                .entry(base.clone())
                .or_insert_with(Vec::new)
                .push(variants.len());
            variants.push(Variant {
                base: base.clone(),
                scope,
                body,
                bound,
                below,
            });
        }
    }
    let config = policy(&doc)?;
    let limit = doc["settings"]["peersSuffixMaxLength"]
        .as_u64()
        .unwrap_or(1000);
    use super::super::identity::{self, Identity, Provider};
    let nodes = variants
        .iter()
        .enumerate()
        .map(|(id, variant)| {
            let patch = reconcile::desired_patch(&variant.base, &doc, [&doc; 4])
                .ok_or_else(|| reconcile::missing(&[&variant.base], "ambiguous patch"))?;
            let providers = variant
                .bound
                .iter()
                .map(|(name, pin)| {
                    let provider = choose(name, pin, &variant.scope, &groups, &variants)
                        .map(Provider::Package)
                        .unwrap_or_else(|| Provider::Link(raw_ref(pin).to_owned()));
                    (name.clone(), provider)
                })
                .collect();
            Ok((
                id,
                Identity {
                    base: &variant.base,
                    patch,
                    providers,
                    children: BTreeMap::new(),
                    declared: &Value::Null,
                },
            ))
        })
        .collect::<Result<_>>()?;
    let keys = identity::encode(&nodes, &config, limit);
    let resolve = |name: &str, reference: &Value, scope: &BTreeMap<String, Value>| {
        choose(name, reference, scope, &groups, &variants).map_or_else(
            || raw_ref(reference).to_owned(),
            |id| reconcile::reference(name, &keys[&id]),
        )
    };
    let mut transitive = Vec::new();
    let mut children = Vec::new();
    for (id, variant) in variants.iter().enumerate() {
        let mut body = variant.body.clone();
        let mut descendants = Vec::new();
        for group in ["dependencies", "optionalDependencies"] {
            if let Some(deps) = body.get_mut(group).and_then(Value::as_object_mut) {
                for (name, pin) in deps {
                    if let Some(child) = choose(name, pin, &variant.scope, &groups, &variants) {
                        descendants.push(child);
                    }
                    *pin = resolve(name, pin, &variant.scope).into();
                }
            }
        }
        let mut names = variant.below.clone();
        if let Some(unbound) = body
            .as_object_mut()
            .unwrap()
            .remove("unboundPeerDependencies")
        {
            names.extend(
                serde_json::from_value::<Vec<String>>(unbound)
                    .map_err(|e| reconcile::missing(&[&variant.base], e.to_string()))?,
            );
        }
        let raw = &doc["packages"][&variant.base];
        for (name, pin) in &variant.bound {
            if !variant.below.contains(name) {
                let group = if raw["peerDependenciesMeta"][name]["optional"] == true {
                    "optionalDependencies"
                } else {
                    "dependencies"
                };
                body[group][name] = resolve(name, pin, &variant.scope).into();
            }
        }
        children.push(descendants);
        transitive.push(names);
        if doc["snapshots"].get(&keys[&id]).is_some() {
            return Err(reconcile::missing(
                &["snapshots", &keys[&id]],
                "multiple v13 variants have the same identity",
            ));
        }
        doc["snapshots"][&keys[&id]] = body;
    }
    // The compact format can leave unbound transitive peer names implicit.
    loop {
        let mut changed = false;
        for (id, variant) in variants.iter().enumerate() {
            if variant.body.get("unboundPeerDependencies").is_some() {
                continue;
            }
            let raw = &doc["packages"][&variant.base];
            let mut add = BTreeSet::new();
            for child in &children[id] {
                let c = &variants[*child];
                let own = graph::section(&doc["packages"][&c.base], "peerDependencies")
                    .into_iter()
                    .flat_map(|m| m.keys());
                for name in own.chain(transitive[*child].iter()) {
                    if !variant.scope.contains_key(name)
                        && raw["peerDependencies"].get(name).is_none()
                    {
                        add.insert(name.clone());
                    }
                }
            }
            let prev = transitive[id].len();
            transitive[id].extend(add);
            changed |= prev != transitive[id].len();
        }
        if !changed {
            break;
        }
    }
    for (id, names) in transitive.into_iter().enumerate() {
        if !names.is_empty() {
            doc["snapshots"][&keys[&id]]["transitivePeerDependencies"] = json!(names);
        }
    }
    for (id, project) in graph::section(next, "projects").into_iter().flatten() {
        let mut importer = project.clone();
        object(project)?;
        importer.as_object_mut().unwrap().remove("name");
        let mut scope = BTreeMap::new();
        for group in manifest::GROUPS {
            for (name, entry) in graph::section(project, group).into_iter().flatten() {
                let spec = entry
                    .as_str()
                    .or_else(|| entry["specifier"].as_str())
                    .ok_or_else(|| {
                        reconcile::missing(&["projects", id, name], "missing specifier")
                    })?;
                let version = if let Some(v) = entry["version"].as_str() {
                    v.to_owned()
                } else {
                    implied(next, &doc, id, name, spec)?
                };
                let mut dep = json!({"specifier":spec,"version":version});
                if let Some(with) = entry.get("with") {
                    dep["with"] = with.clone();
                }
                scope.insert(name.clone(), dep.clone());
                importer[group][name] = dep;
            }
        }
        for group in manifest::GROUPS {
            if let Some(deps) = importer.get_mut(group).and_then(Value::as_object_mut) {
                for (name, entry) in deps {
                    entry["version"] = resolve(name, entry, &scope).into();
                    entry.as_object_mut().unwrap().remove("with");
                }
            }
        }
        doc["importers"][id] = importer;
    }
    Ok(doc)
}

fn implied(next: &Value, doc: &Value, id: &str, name: &str, spec: &str) -> Result<String> {
    if let Some(c) = spec.strip_prefix("catalog:") {
        let entry = &doc["catalogs"][if c.is_empty() { "default" } else { c }][name];
        let pin = entry["version"]
            .as_str()
            .ok_or_else(|| reconcile::missing(&[id, name], "missing catalog entry"))?;
        return Ok(
            reconcile::alias(entry["specifier"].as_str().unwrap_or_default())
                .map_or_else(|| pin.into(), |(actual, _)| format!("{actual}@{pin}")),
        );
    }
    if let Some((actual, pin)) = reconcile::alias(spec) {
        return Ok(format!("{actual}@{pin}"));
    }
    if let Some(workspace) = spec.strip_prefix("workspace:") {
        let target = workspace
            .rsplit_once('@')
            .filter(|(n, _)| !n.is_empty())
            .map_or(name, |(n, _)| n);
        if let Some((_, p)) = graph::section(doc, "packages")
            .into_iter()
            .flatten()
            .find(|(key, _)| key.starts_with(&format!("{target}@file:")))
            && let Some(dir) = p["resolution"]["directory"].as_str()
        {
            return Ok(reconcile::reference(name, &format!("{target}@file:{dir}")));
        }
        let linked = graph::section(next, "projects")
            .into_iter()
            .flatten()
            .find(|(_, p)| p["name"] == target)
            .map(|(d, _)| d.as_str());
        if workspace.starts_with('.') {
            return Ok(format!("link:{}", workspace.trim_start_matches("./")));
        }
        if let Some(linked) = linked {
            return Ok(format!(
                "link:{}",
                crate::workspace_links::relative(id, linked)
            ));
        }
        return Err(reconcile::missing(&[id, name], "missing workspace target"));
    }
    Ok(spec.into())
}
