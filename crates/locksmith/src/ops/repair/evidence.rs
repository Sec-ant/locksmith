//! Obtain individual missing records from compatible parents or a resolved graph.
use super::RepairContext;
use crate::{
    Lockfile, LockfileError, MergeContext, MergeTarget, Value, WorkspaceConfig,
    model::error::missing, model::graph, model::manifest, model::patch, model::pin, model::schema,
    resolve,
};
use serde_json::json;

pub(super) struct Evidence<'a> {
    pub(super) lock: &'a Lockfile,
    pub(super) context: &'a RepairContext<'a>,
    resolved: Option<Lockfile>,
    published: Option<Value>,
}

impl<'a> Evidence<'a> {
    pub(super) fn new(lock: &'a Lockfile, context: &'a RepairContext<'a>) -> Self {
        Self {
            lock,
            context,
            resolved: None,
            published: None,
        }
    }

    pub(super) fn merge_context(
        &self,
        configurations: [&'a WorkspaceConfig; 3],
    ) -> MergeContext<'a> {
        MergeContext {
            target: MergeTarget::Ours,
            manifests: self.context.manifests,
            configurations,
            configuration: self.context.configuration,
            patches: self.context.patches,
            registry: self.context.registry,
        }
    }

    pub(super) fn resolved(&mut self) -> Result<&Value, LockfileError> {
        if self.context.parents.len() > 2 {
            return Err(missing(
                &["parents"],
                "resolving this dependency requires a two-parent merge",
            ));
        }
        if self.resolved.is_none() {
            let current = (self.lock, self.context.configuration);
            let (base, b) = self.context.parents.first().copied().unwrap_or(current);
            let (other, t) = self.context.parents.get(1).copied().unwrap_or(current);
            let context = self.merge_context([b, current.1, t]);
            self.resolved = Some(resolve::resolve_project(
                [base, self.lock, other],
                &context,
            )?);
        }
        Ok(self
            .resolved
            .as_ref()
            .unwrap()
            .project()
            .expect("resolved project"))
    }

    pub(super) fn record(&mut self, section: &str, key: &str) -> Result<Value, LockfileError> {
        let current = self.lock.project().expect("project");
        let mut agreed = None;
        for &(lock, config) in self.context.parents {
            let Some(parent) = lock.project() else {
                continue;
            };
            if config.resolution != self.context.configuration.resolution
                || config.override_tie_break != self.context.configuration.override_tie_break
                || !manifest::same_overrides(current, parent)
                || !patch::records_match(current, schema::rule_map(parent, "patchedDependencies"))
            {
                continue;
            }
            let Some(record) = parent[section].get(key) else {
                continue;
            };
            let package = pin::package_key(key);
            if section == "snapshots"
                && let Some(resolution) = current["packages"][package].get("resolution")
                && parent["packages"][package].get("resolution") != Some(resolution)
            {
                return Err(missing(
                    &["packages", package, "resolution"],
                    "parent snapshot belongs to a different package artifact",
                ));
            }
            if agreed.is_some_and(|first| first != record) {
                return Err(missing(
                    &[section, key],
                    "parents disagree on the missing record",
                ));
            }
            agreed = Some(record);
        }
        if let Some(record) = agreed {
            return Ok(record.clone());
        }
        let package = pin::package_key(key);
        if let Some(publication) = self.context.registry.and_then(|r| r.releases.get(package)) {
            if self.published.is_none() {
                let config = self.context.configuration;
                let ctx = self.merge_context([config; 3]);
                self.published = Some(resolve::published::records(
                    &ctx,
                    current,
                    [current; 3],
                    [0, 1, 2],
                    None,
                )?);
            }
            let published = self.published.as_ref().expect("published records");
            if section == "packages"
                && let Some(record) = published["packages"].get(key)
            {
                return Ok(record.clone());
            }
            if section == "snapshots"
                && let Some(record) = peer_only_snapshot(key, &publication.manifest, published)
            {
                return Ok(record);
            }
        } else if current["packages"].get(package).is_none()
            && self.context.parents.iter().all(|(lock, _)| {
                lock.project()
                    .and_then(|p| p["packages"].get(package))
                    .is_none()
            })
            && let Some(crate::RegistryRequest::Manifest { name, version }) =
                crate::RegistryRequest::manifest(key)
        {
            return Err(resolve::published::need_version(
                &[section, key],
                &name,
                &version,
                self.context.configuration,
                "missing locked release".into(),
            ));
        }
        self.resolved()?[section].get(key).cloned().ok_or_else(|| {
            missing(
                &[section, key],
                "no matching record with this exact peer and patch identity",
            )
        })
    }
}

// For a published leaf whose only edges are peers, the explicit lock key
// supplies the whole binding environment. Hashes and transitive peer scopes
// require a witnessed instance or the contextual resolver.
fn peer_only_snapshot(key: &str, release: &Value, published: &Value) -> Option<Value> {
    let pkg = pin::package_key(key);
    if ["dependencies", "optionalDependencies"]
        .iter()
        .any(|field| graph::section(release, field).is_some_and(|m| !m.is_empty()))
    {
        return None;
    }
    let package = published["packages"].get(pkg)?;
    let mut bindings = crate::peer::explicit_bindings(key)?;
    let hash = bindings.remove("patch_hash");
    let template = hash.map_or_else(|| pkg.to_owned(), |h| format!("{pkg}(patch_hash={h})"));
    let skeleton = published["snapshots"].get(&template)?;
    let peers = graph::section(package, "peerDependencies");
    if ["dependencies", "optionalDependencies"]
        .iter()
        .flat_map(|g| {
            graph::section(skeleton, g)
                .into_iter()
                .flat_map(|m| m.keys())
        })
        .any(|name| !peers.is_some_and(|p| p.contains_key(name)))
    {
        return None;
    }
    let mut result = json!({});
    for (name, specifier) in peers.into_iter().flatten() {
        let optional = package["peerDependenciesMeta"][name]["optional"] == true;
        let Some(version) = bindings.remove(name.as_str()) else {
            if optional { continue } else { return None }
        };
        if !pin::matches_version(specifier.as_str()?, Some(pin::package_key(version))) {
            return None;
        }
        let group = if optional {
            "optionalDependencies"
        } else {
            "dependencies"
        };
        result[group][name] = json!(version);
    }
    bindings.is_empty().then_some(result)
}
