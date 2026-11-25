//! Resolve local dependencies against the final workspace manifests.
use crate::{LockfileError, Value, WorkspaceConfig, graph, manifest, reconcile::missing};
use std::collections::{BTreeMap, HashMap};

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

fn workspace_request<'a>(name: &'a str, request: &'a str) -> (&'a str, &'a str) {
    request
        .rsplit_once('@')
        .filter(|(n, _)| !n.is_empty())
        .unwrap_or((name, request))
}

fn workspace_version_matches(selector: &str, version: Option<&str>) -> bool {
    if matches!(selector, "" | "*" | "^" | "~") {
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

    pub(crate) fn check_pin(
        &self,
        importer: &str,
        name: &str,
        specifier: &str,
        path: &str,
    ) -> Result<(), LockfileError> {
        let effective = self.catalog_specifier(name, specifier)?;
        let Some(request) = effective.strip_prefix("workspace:").map(str::trim) else {
            return Ok(());
        };
        let target = graph::linked_importer(importer, path);
        let fits = if request.starts_with('.') {
            target == graph::linked_importer(importer, request)
        } else {
            let (package, range) = workspace_request(name, request);
            self.by_name.get(package).into_iter().flatten().any(|&id| {
                let member = &self.manifests[id];
                link_directory(id, member) == target
                    && workspace_version_matches(range, member["version"].as_str())
            })
        };
        if fits {
            Ok(())
        } else {
            Err(manifest::mismatch(
                importer,
                "workspace",
                format!("{name}: link:{path} does not satisfy {effective}"),
            ))
        }
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
        let link_local = self
            .config
            .resolution
            .parameters
            .get("linkWorkspacePackages");
        if workspace.is_none()
            && explicit_link.is_none()
            && !link_local.is_some_and(|v| v == true || v == "deep")
        {
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
                let (name, range) = manifest::alias(specifier).unwrap_or((name, specifier));
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
