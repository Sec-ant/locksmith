//! Resolve local dependencies against the final workspace manifests.
use crate::{
    LockfileError, Value, WorkspaceConfig, graph, manifest,
    reconcile::{alias, missing},
};
use std::collections::{BTreeMap, HashMap};

pub(crate) fn relative(from: &str, to: &str) -> String {
    let a: Vec<_> = from
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let b: Vec<_> = to
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    std::iter::repeat_n("..", a.len() - common)
        .chain(b[common..].iter().copied())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn member_relative(from: &str, to: &str, config: &WorkspaceConfig) -> String {
    if let Some(root) = config.lockfile_dir.as_deref()
        && (from.starts_with("..") || to.starts_with(".."))
    {
        relative(
            &graph::linked_importer(root, from),
            &graph::linked_importer(root, to),
        )
    } else {
        relative(from, to)
    }
}

pub(crate) fn member_target(from: &str, path: &str, config: &WorkspaceConfig) -> String {
    if let Some(root) = config.lockfile_dir.as_deref()
        && (from.starts_with("..") || path.starts_with(".."))
    {
        let target = graph::linked_importer(&graph::linked_importer(root, from), path);
        // The target is normalized, so the root must be too (`/repo/sub/..`).
        let id = relative(&graph::linked_importer(root, "."), &target);
        if id.is_empty() { ".".into() } else { id }
    } else {
        graph::linked_importer(from, path)
    }
}

pub(crate) fn injected(name: &str, manifest: &Value, configuration: &WorkspaceConfig) -> bool {
    manifest
        .get("dependenciesMeta")
        .and_then(|v| v.get(name))
        .and_then(|v| v.get("injected"))
        .and_then(Value::as_bool)
        .unwrap_or(configuration.resolution.inject_workspace_packages)
}

pub(crate) fn prefers_workspace(config: &WorkspaceConfig) -> bool {
    config
        .resolution
        .parameters
        .get("preferWorkspacePackages")
        .is_some_and(|v| v == true)
}

pub(crate) fn prefers_workspace_version(
    config: &WorkspaceConfig,
    workspace: &Value,
    registry_version: &str,
) -> bool {
    let versions = workspace["version"]
        .as_str()
        .and_then(|v| v.parse::<node_semver::Version>().ok())
        .zip(registry_version.parse::<node_semver::Version>().ok());
    versions.is_some_and(|(local, registry)| local >= registry || prefers_workspace(config))
}

pub(crate) struct WorkspaceLinks<'a> {
    manifests: &'a BTreeMap<String, Value>,
    config: &'a WorkspaceConfig,
    by_name: HashMap<&'a str, Option<&'a str>>,
}

impl<'a> WorkspaceLinks<'a> {
    pub(crate) fn new(manifests: &'a BTreeMap<String, Value>, config: &'a WorkspaceConfig) -> Self {
        let mut by_name = HashMap::new();
        for (id, package) in manifests {
            if let Some(name) = package["name"].as_str() {
                by_name
                    .entry(name)
                    .and_modify(|id| *id = None)
                    .or_insert(Some(id.as_str()));
            }
        }
        Self {
            manifests,
            config,
            by_name,
        }
    }

    pub(crate) fn resolve(
        &self,
        name: &str,
        specifier: &str,
        importer: &str,
        manifest: &Value,
    ) -> Result<Option<(String, Option<&'a Value>)>, LockfileError> {
        self.select(name, specifier, importer, manifest, false, true)
    }

    pub(crate) fn resolve_for_generation(
        &self,
        name: &str,
        specifier: &str,
        importer: &str,
        manifest: &Value,
        direct: bool,
    ) -> Result<Option<(String, Option<&'a Value>)>, LockfileError> {
        self.select(name, specifier, importer, manifest, true, direct)
    }

    fn select(
        &self,
        name: &str,
        specifier: &str,
        importer: &str,
        manifest: &Value,
        include_injected: bool,
        direct: bool,
    ) -> Result<Option<(String, Option<&'a Value>)>, LockfileError> {
        let specifier = if let Some(catalog) = specifier.strip_prefix("catalog:") {
            let catalog = if catalog.is_empty() {
                "default"
            } else {
                catalog
            };
            self.config
                .catalogs
                .get(catalog)
                .and_then(|entries| entries.get(name))
                .ok_or_else(|| {
                    missing(
                        &["catalogs", catalog, name],
                        "final workspace has no declaration",
                    )
                })?
                .as_str()
        } else {
            specifier
        };
        let workspace = specifier.strip_prefix("workspace:").map(str::trim);
        let explicit_link = specifier.strip_prefix("link:");
        let link_local = self
            .config
            .resolution
            .parameters
            .get("linkWorkspacePackages");
        if workspace.is_none()
            && explicit_link.is_none()
            && !link_local.is_some_and(|v| (direct && v == true) || v == "deep")
        {
            return Ok(None);
        }
        if !include_injected && injected(name, manifest, self.config) && explicit_link.is_none() {
            return Ok(None);
        }
        let relative = explicit_link
            .filter(|p| !p.starts_with('/'))
            .or_else(|| workspace.filter(|r| r.starts_with('.')));
        let (target, range) = if let Some(path) = relative {
            (Some(member_target(importer, path, self.config)), "*")
        } else {
            let (package, range) = if let Some(request) = workspace {
                request
                    .rsplit_once('@')
                    .filter(|(n, _)| !n.is_empty())
                    .unwrap_or((name, request))
            } else {
                let (name, range) = alias(specifier).unwrap_or((name, specifier));
                if range.parse::<node_semver::Range>().is_err() {
                    return Ok(None);
                }
                (name, range)
            };
            let first = match self.by_name.get(package) {
                Some(Some(id)) => Some((*id).to_owned()),
                Some(None) => {
                    return Err(missing(
                        &["importers", importer, name],
                        "workspace package name is ambiguous",
                    ));
                }
                None => None,
            };
            (first, range)
        };
        let Some(target) = target else {
            return Ok(None);
        };
        let package = self.manifests.get(&target);
        let target = if relative.is_some() {
            // Path specifiers link the declared directory, including non-members.
            target
        } else {
            let Some(package) = package else {
                return Ok(None);
            };
            let accepts_any = workspace.is_some() && matches!(range, "" | "*" | "^" | "~");
            if !(accepts_any || manifest::matches_version(range, package["version"].as_str())) {
                return Ok(None);
            }
            let publish = &package["publishConfig"];
            if publish["linkDirectory"] != false
                && let Some(directory) = publish["directory"].as_str()
            {
                graph::linked_importer(&target, directory)
            } else {
                target
            }
        };
        if include_injected {
            return Ok(Some((format!("link:{target}"), package)));
        }
        let relative = member_relative(importer, &target, self.config);
        Ok(Some((format!("link:{relative}"), package)))
    }
}
