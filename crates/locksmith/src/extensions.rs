//! Repair ordinary dependencies declared by a package extension when the final
//! workspace overrides their version independently of the published range.
use std::collections::{BTreeSet, HashMap};

use crate::{
    LockfileError, Value, WorkspaceConfig, catalog, config, graph, manifest, patch, peer, pin,
};

#[derive(Default)]
pub(crate) struct Plan {
    edges: Vec<(String, &'static str, String, String)>,
    records: Vec<(&'static str, String, Value)>,
}

impl Plan {
    pub(crate) fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Returns whether the plan added records whose own constraints need checking.
    pub(crate) fn apply(self, doc: &mut Value) -> bool {
        let added = !self.records.is_empty();
        for (section, key, value) in self.records {
            doc[section][key] = value;
        }
        for (key, group, name, pin) in self.edges {
            doc["snapshots"][key][group][name] = pin.into();
        }
        added
    }
}

/// Find exact, consumer-specific corrections. An installed lock can also
/// witness records discarded when convergence selected a baseline subtree.
pub(crate) fn plan(
    doc: &Value,
    cfg: &WorkspaceConfig,
    installed: Option<&Value>,
    reachable: Option<&graph::DependencyClosure>,
) -> Result<Plan, LockfileError> {
    let mut plan = Plan::default();
    let Some(extensions) = cfg
        .resolution
        .package_extensions
        .as_ref()
        .and_then(Value::as_object)
    else {
        return Ok(plan);
    };
    if extensions.is_empty()
        || cfg.resolution.pnpmfile_checksum.is_some()
        || graph::section(doc, "overrides").is_none_or(|v| v.is_empty())
    {
        return Ok(plan);
    }

    let mut rules: HashMap<_, Vec<_>> = HashMap::new();
    for (selector, body) in extensions {
        let (_, name, _) = manifest::override_selector(selector);
        rules
            .entry(name)
            .or_default()
            .push((selector.as_str(), body));
    }
    let overrides = manifest::Overrides::new(doc, cfg);
    let mut constraints = HashMap::new();
    for (release, pkg) in graph::section(doc, "packages").into_iter().flatten() {
        if reachable.is_some_and(|c| !c.packages.contains(release)) {
            continue;
        }
        let Some((parent, version)) = release.rsplit_once('@') else {
            continue;
        };
        let Some(rules) = rules.get(parent) else {
            continue;
        };
        let fields = config::extension_fields(rules, parent, version).ok_or_else(|| {
            crate::error::missing(
                &["packageExtensions", release],
                "expected dependency mappings",
            )
        })?;
        let identity = serde_json::json!({"name":parent,"version":version});
        for (name, value) in &fields[0] {
            if fields[1].contains_key(name)
                || fields[2].contains_key(name)
                || pkg["peerDependencies"].get(*name).is_some()
            {
                continue;
            }
            let relevant = overrides.relevant(name, parent, version);
            if relevant.is_empty()
                || relevant
                    .iter()
                    .any(|(s, _)| manifest::override_selector(s).2.is_some())
            {
                continue;
            }
            let Some(declaration) = value.as_str() else {
                continue;
            };
            if let Some(spec) = overrides.apply(&identity, ".", name, declaration)?
                && pin::alias(&spec)
                    .map_or(spec.as_ref(), |(_, v)| v)
                    .parse::<node_semver::Version>()
                    .is_ok()
            {
                constraints
                    .entry(release.as_str())
                    .or_insert_with(Vec::new)
                    .push((*name, spec));
            }
        }
    }
    if constraints.is_empty() {
        return Ok(plan);
    }
    let peers = peer::Merger::new([Some(doc), installed, None]);
    let mut needed = BTreeSet::new();
    for (key, body) in graph::section(doc, "snapshots").into_iter().flatten() {
        if reachable.is_some_and(|c| !c.snapshots.contains(key)) {
            continue;
        }
        let Some(targets) = constraints.get(pin::package_key(key)) else {
            continue;
        };
        if patch::hash_in_key(key).is_some() {
            continue;
        }
        let mut changes = Vec::new();
        for (name, spec) in targets {
            let edge = graph::dependency(body, name);
            if edge.is_some_and(|(_, pin)| catalog::accepts_pin(name, spec, pin, doc, Some(cfg))) {
                continue;
            }
            let pin = peers
                .exact_override([key], cfg, name, spec)
                .or_else(|| {
                    let donor = installed?;
                    let (_, selected) = graph::dependency(donor["snapshots"].get(key)?, name)?;
                    let pin = peers.exact_override([key, key], cfg, name, spec)?;
                    (pin == selected).then_some(pin)
                })
                .ok_or_else(|| {
                    crate::error::missing(
                        &["snapshots", key, name],
                        format!(
                            "no unique locked instance of {spec} matches this consumer's peers"
                        ),
                    )
                })?;
            if let Some(donor) = installed
                && doc["snapshots"]
                    .get(graph::snapshot_key(
                        name,
                        &pin,
                        graph::section(doc, "snapshots"),
                    ))
                    .is_none()
            {
                needed.insert(graph::snapshot_key(
                    name,
                    &pin,
                    graph::section(donor, "snapshots"),
                ));
            }
            changes.push((edge.map_or("dependencies", |(g, _)| g), *name, pin));
        }
        for (_, name, pin) in &changes {
            if graph::transitive(body).any(|peer| peer == *name)
                || peer::explicit_bindings(key).is_some_and(|m| m.contains_key(name))
            {
                return Err(crate::error::missing(
                    &["snapshots", key, name],
                    "the override also changes the parent’s peer identity",
                ));
            }
            // Check the final set of siblings, including other edges repaired in
            // this same record, before reusing the parent's existing identity.
            for group in ["dependencies", "optionalDependencies"] {
                for (sibling, value) in graph::section(body, group).into_iter().flatten() {
                    if sibling == name || changes.iter().any(|(_, edited, _)| edited == sibling) {
                        continue;
                    }
                    if let Some(reference) = value.as_str() {
                        verify_provider(
                            [doc, installed.unwrap_or(doc)],
                            key,
                            sibling,
                            reference,
                            name,
                            pin,
                        )?;
                    }
                }
            }
            for (_, sibling, reference) in &changes {
                if sibling != name {
                    verify_provider(
                        [doc, installed.unwrap_or(doc)],
                        key,
                        sibling,
                        reference,
                        name,
                        pin,
                    )?;
                }
            }
        }
        plan.edges.extend(
            changes
                .into_iter()
                .map(|(group, name, pin)| (key.clone(), group, name.to_owned(), pin)),
        );
    }
    if !needed.is_empty() {
        let donor = installed.expect("missing targets came from the installed lock");
        let closure = graph::closure_from_edges(donor, &serde_json::json!({}), &needed)?;
        for (section, keys) in [
            ("packages", closure.packages),
            ("snapshots", closure.snapshots),
        ] {
            for key in keys {
                let record = &donor[section][&key];
                match doc[section].get(&key) {
                    Some(existing) if existing != record => {
                        return Err(crate::error::missing(
                            &[section, &key],
                            "the installed override requires a different record for a shared identity",
                        ));
                    }
                    Some(_) => (),
                    None => plan.records.push((section, key, record.clone())),
                }
            }
        }
    }
    Ok(plan)
}

fn verify_provider(
    [primary, donor]: [&Value; 2],
    parent: &str,
    sibling: &str,
    reference: &str,
    name: &str,
    pin: &str,
) -> Result<(), LockfileError> {
    let primary_key = graph::snapshot_key(sibling, reference, graph::section(primary, "snapshots"));
    let (doc, key) = if primary["snapshots"].get(&primary_key).is_some() {
        (primary, primary_key)
    } else {
        (
            donor,
            graph::snapshot_key(sibling, reference, graph::section(donor, "snapshots")),
        )
    };
    let body = &doc["snapshots"][&key];
    let declares_peer = doc["packages"][pin::package_key(&key)]["peerDependencies"]
        .get(name)
        .is_some();
    if declares_peer || graph::transitive(body).any(|n| n == name) {
        let bound = if declares_peer {
            graph::dependency(body, name).map(|(_, v)| v)
        } else {
            peer::explicit_bindings(&key).and_then(|b| b.get(name).copied())
        };
        if bound != Some(pin) {
            return Err(crate::error::missing(
                &["snapshots", parent, sibling],
                format!("peer {name} also needs rebinding to {pin}"),
            ));
        }
    }
    Ok(())
}
