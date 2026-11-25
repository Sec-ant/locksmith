//! Resolve local dependencies against the final workspace manifests.
use crate::{LockfileError, Value, WorkspaceConfig, graph, manifest, reconcile::missing};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(crate) fn injected(name: &str, manifest: &Value, configuration: &WorkspaceConfig) -> bool {
    manifest
        .get("dependenciesMeta")
        .and_then(|v| v.get(name))
        .and_then(|v| v.get("injected"))
        .and_then(Value::as_bool)
        .unwrap_or(configuration.resolution.inject_workspace_packages)
}

pub(crate) struct WorkspaceLinks<'a> {
    manifests: &'a BTreeMap<String, Value>,
    config: &'a WorkspaceConfig,
    external_members: Option<&'a BTreeSet<String>>,
    by_name: HashMap<&'a str, Vec<&'a str>>,
}

pub(crate) fn workspace_request<'a>(name: &'a str, request: &'a str) -> (&'a str, &'a str) {
    request
        .rsplit_once('@')
        .filter(|(n, _)| !n.is_empty())
        .unwrap_or((name, request))
}

fn local_version_matches(selector: &str, version: Option<&str>) -> bool {
    if matches!(selector, "" | "*") {
        return true;
    }
    match selector.parse::<node_semver::Range>() {
        Ok(range) => version
            .unwrap_or("0.0.0")
            .parse::<node_semver::Version>()
            .is_ok_and(|version| version.satisfies(&range)),
        // pnpm accepts a tag when encodeURIComponent leaves the selector unchanged.
        Err(_) => selector
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&c)),
    }
}

fn link_directory(id: &str, package: &Value) -> String {
    let publish = &package["publishConfig"];
    if publish["linkDirectory"] != false
        && let Some(directory) = publish["directory"].as_str()
    {
        graph::linked_importer(id, directory)
    } else {
        id.to_owned()
    }
}

impl<'a> WorkspaceLinks<'a> {
    pub(crate) fn new(manifests: &'a BTreeMap<String, Value>, config: &'a WorkspaceConfig) -> Self {
        let mut by_name = HashMap::<_, Vec<_>>::new();
        for (id, package) in manifests {
            if let Some(name) = package["name"].as_str() {
                by_name.entry(name).or_default().push(id.as_str());
            }
        }
        Self {
            manifests,
            config,
            by_name,
            external_members: None,
        }
    }

    pub(crate) fn for_validation(
        manifests: &'a BTreeMap<String, Value>,
        config: &'a WorkspaceConfig,
        external: &'a BTreeSet<String>,
    ) -> Self {
        let mut links = Self::new(manifests, config);
        // Since 12.15 an in-root alias cannot resolve a named pin outside the root.
        // An explicit workspace path can still point to an external directory.
        if !external.is_empty()
            && config
                .pnpm_version
                .as_ref()
                .is_some_and(|v| v >= &node_semver::Version::from((12, 15, 0)))
        {
            for candidates in links.by_name.values_mut() {
                candidates.retain(|id| !external.contains(*id));
            }
        }
        links.external_members = Some(external);
        links
    }

    pub(crate) fn can_omit_empty_importer(&self, id: &str) -> bool {
        self.external_members
            .is_some_and(|external| external.contains(id))
            && self
                .config
                .pnpm_version
                .as_ref()
                .is_none_or(|v| v.major() < 12 || v >= &node_semver::Version::from((12, 8, 0)))
    }

    fn catalog_specifier<'s>(
        &'s self,
        name: &str,
        specifier: &'s str,
    ) -> Result<&'s str, LockfileError> {
        let Some(catalog) = specifier.strip_prefix("catalog:") else {
            return Ok(specifier);
        };
        let catalog = if catalog.is_empty() {
            "default"
        } else {
            catalog
        };
        self.config
            .catalogs
            .get(catalog)
            .and_then(|entries| entries.get(name))
            .map(String::as_str)
            .ok_or_else(|| {
                missing(
                    &["catalogs", catalog, name],
                    "final workspace has no declaration",
                )
            })
    }

    pub(crate) fn check_pin(
        &self,
        document: &Value,
        importer: &str,
        name: &str,
        specifier: &str,
        version: &str,
    ) -> Result<(), LockfileError> {
        let effective = self.catalog_specifier(name, specifier)?;

        let mismatch = || {
            manifest::mismatch(
                importer,
                "workspace",
                format!("{name}: {version} does not satisfy {effective}"),
            )
        };
        let Some(request) = effective.strip_prefix("workspace:").map(str::trim) else {
            if let Some(path) = version.strip_prefix("link:")
                && self.links_workspace_packages()
                && let Some(v) = self.config.pnpm_version.as_ref()
                && (v.major() == 11 && v >= &node_semver::Version::from((11, 28, 0))
                    || v >= &node_semver::Version::from((12, 19, 0)))
            {
                let (package, range) = manifest::registry_request(name, effective);
                if range.parse::<node_semver::Range>().is_ok() {
                    let id = graph::linked_project(document.get("importers"), importer, path);
                    if let Some(member) = self.manifests.get(&id)
                        && (!local_version_matches(range, member["version"].as_str())
                            || v.major() >= 12 && member["name"] != package)
                    {
                        return Err(mismatch());
                    }
                }
            }
            return Ok(());
        };
        let (target, pinned_name) = if let Some(path) = version.strip_prefix("link:") {
            (graph::linked_importer(importer, path), None)
        } else {
            let (package, path) = graph::file_pin(name, version).ok_or_else(mismatch)?;
            let resolution = &document["packages"][format!("{package}@file:{path}")]["resolution"];
            let directory = resolution["directory"].as_str().ok_or_else(mismatch)?;
            let target = graph::linked_importer(".", path);
            if resolution["type"] != "directory" || graph::linked_importer(".", directory) != target
            {
                return Err(mismatch());
            }
            (target, Some(package))
        };
        let fits = if request.starts_with('.') {
            target == graph::linked_importer(importer, request)
                && pinned_name
                    .zip(self.manifests.get(&target).and_then(|m| m["name"].as_str()))
                    .is_none_or(|(actual, expected)| actual == expected)
        } else {
            let (package, range) = workspace_request(name, request);
            pinned_name.is_none_or(|n| n == package)
                && self.by_name.get(package).into_iter().flatten().any(|&id| {
                    let member = &self.manifests[id];
                    link_directory(id, member) == target
                        && (matches!(range, "^" | "~")
                            || local_version_matches(range, member["version"].as_str()))
                })
        };
        if fits { Ok(()) } else { Err(mismatch()) }
    }

    fn links_workspace_packages(&self) -> bool {
        self.config
            .resolution
            .parameters
            .get("linkWorkspacePackages")
            .is_some_and(|v| v == true || v == "deep")
    }

    pub(crate) fn can_omit(&self, name: &str, specifier: &str) -> bool {
        if !self.links_workspace_packages()
            || !self
                .config
                .supports_omitted_workspace_link(specifier.starts_with("catalog:"))
        {
            return false;
        }
        let Ok(specifier) = self.catalog_specifier(name, specifier) else {
            return false;
        };
        if specifier.starts_with("workspace:") {
            return false;
        }
        let (package, selector) = manifest::registry_request(name, specifier);
        self.by_name
            .get(package)
            .into_iter()
            .flatten()
            .any(|&id| local_version_matches(selector, self.manifests[id]["version"].as_str()))
    }

    pub(crate) fn resolve(
        &self,
        name: &str,
        specifier: &str,
        importer: &str,
        manifest: &Value,
    ) -> Result<Option<(String, Option<&'a Value>)>, LockfileError> {
        let specifier = self.catalog_specifier(name, specifier)?;
        let workspace = specifier.strip_prefix("workspace:").map(str::trim);
        let explicit_link = specifier.strip_prefix("link:");
        if workspace.is_none() && explicit_link.is_none() && !self.links_workspace_packages() {
            return Ok(None);
        }
        if injected(name, manifest, self.config) && explicit_link.is_none() {
            return Ok(None);
        }
        let relative = explicit_link
            .filter(|p| !p.starts_with('/'))
            .or_else(|| workspace.filter(|r| r.starts_with('.')));
        let (target, range) = if let Some(path) = relative {
            (Some(graph::linked_importer(importer, path)), "*")
        } else {
            let (package, range) = if let Some(request) = workspace {
                workspace_request(name, request)
            } else {
                let (name, range) = manifest::registry_request(name, specifier);
                if range.parse::<node_semver::Range>().is_err() {
                    return Ok(None);
                }
                (name, range)
            };
            let first = match self.by_name.get(package).map(Vec::as_slice) {
                Some([id]) => Some((*id).to_owned()),
                Some(_) => {
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
            link_directory(&target, package)
        };
        let from: Vec<_> = importer
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        let to: Vec<_> = target
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
        let relative = std::iter::repeat_n("..", from.len() - common)
            .chain(to[common..].iter().copied())
            .collect::<Vec<_>>()
            .join("/");
        Ok(Some((format!("link:{relative}"), package)))
    }
}
