//! Resolve local dependencies against the final workspace manifests.
use crate::{
    LockfileError, Value, WorkspaceConfig, error::missing, graph, manifest, pin,
    pin::workspace_request,
};
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
    by_name: HashMap<&'a str, Vec<&'a str>>,
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

type LinkedManifest<'a> = dyn Fn(&str) -> Result<Option<Value>, LockfileError> + 'a;

pub(crate) struct FrozenWorkspace<'a> {
    read_linked: &'a LinkedManifest<'a>,
    pub links: WorkspaceLinks<'a>,
    projects: graph::LinkedProjects<'a>,
    external_members: &'a BTreeSet<String>,
}

impl<'a> FrozenWorkspace<'a> {
    pub(crate) fn new(
        manifests: &'a BTreeMap<String, Value>,
        config: &'a WorkspaceConfig,
        document: &'a Value,
        external: &'a BTreeSet<String>,
        read_linked: &'a LinkedManifest<'a>,
    ) -> Self {
        Self {
            links: WorkspaceLinks::new(manifests, config),
            projects: graph::LinkedProjects::new(document.get("importers")),
            external_members: external,
            read_linked,
        }
    }

    pub(crate) fn check_pin(
        &self,
        document: &Value,
        importer: &str,
        name: &str,
        specifier: &str,
        version: &str,
    ) -> Result<(), LockfileError> {
        let effective = self.links.catalog_specifier(name, specifier)?;

        let mismatch = || {
            manifest::mismatch(
                importer,
                "workspace",
                format!("{name}: {version} does not satisfy {effective}"),
            )
        };
        let Some(request) = effective.strip_prefix("workspace:").map(str::trim) else {
            if let Some(path) = version.strip_prefix("link:")
                && self.links.links_workspace_packages()
                && self.links.config.since((11, 28, 0), (12, 19, 0))
            {
                let (package, range) = self.links.config.registry_request(name, effective);
                if range.parse::<node_semver::Range>().is_ok() {
                    let target = self.projects.resolve(importer, path);
                    if !self.range_link_matches(&target, package, range)? {
                        return Err(mismatch());
                    }
                }
            }
            return Ok(());
        };
        let (target, pinned_name) = if let Some(path) = version.strip_prefix("link:") {
            (graph::linked_importer(importer, path), None)
        } else {
            let (package, path) = pin::file_pin(name, version).ok_or_else(mismatch)?;
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
                    .zip(
                        self.links
                            .manifests
                            .get(&target)
                            .and_then(|m| m["name"].as_str()),
                    )
                    .is_none_or(|(actual, expected)| actual == expected)
        } else {
            let (package, range) = workspace_request(name, request);
            pinned_name.is_none_or(|n| n == package)
                && self
                    .links
                    .by_name
                    .get(package)
                    .into_iter()
                    .flatten()
                    .any(|&id| {
                        let member = &self.links.manifests[id];
                        !(self.external_members.contains(id)
                            && self
                                .links
                                .config
                                .pnpm_version
                                .as_ref()
                                .is_some_and(|v| v >= &node_semver::Version::from((12, 15, 0))))
                            && link_directory(id, member) == target
                            && (matches!(range, "^" | "~")
                                || local_version_matches(range, member["version"].as_str()))
                    })
        };
        if fits { Ok(()) } else { Err(mismatch()) }
    }

    fn range_link_matches(&self, id: &str, name: &str, range: &str) -> Result<bool, LockfileError> {
        let modern = self
            .links
            .config
            .pnpm_version
            .as_ref()
            .is_some_and(|v| v.major() >= 12);
        if modern && self.external_members.contains(id) {
            return Ok(false);
        }
        let disk;
        let member = if let Some(member) = self.links.manifests.get(id) {
            member
        } else {
            disk = (self.read_linked)(id)?;
            // pnpm 12 permits a sparse checkout but still checks a present directory.
            let Some(member) = disk.as_ref() else {
                return Ok(modern);
            };
            member
        };
        Ok((!modern || member["name"] == name)
            && local_version_matches(range, member["version"].as_str()))
    }

    pub(crate) fn can_omit_empty_importer(&self, id: &str) -> bool {
        self.external_members.contains(id)
            && self
                .links
                .config
                .pnpm_version
                .as_ref()
                .is_none_or(|v| v.major() < 12 || v >= &node_semver::Version::from((12, 8, 0)))
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
        }
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
        let (package, selector) = self.config.registry_request(name, specifier);
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
                let (name, range) = self.config.registry_request(name, specifier);
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
            if !(accepts_any || pin::matches_version(range, package["version"].as_str())) {
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
