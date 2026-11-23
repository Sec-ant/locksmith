//! pnpm applies its compatibility database and workspace package extensions
//! before handing a manifest to readPackage. The database is indexed by package
//! name. Each edition records the first pnpm release with that set of rules.
use std::{borrow::Cow, collections::BTreeMap, sync::LazyLock};

use node_semver::Version;
use serde_json::Map;

use super::Result;
use crate::{Value, WorkspaceConfig, graph, manifest, reconcile};

type Editions = BTreeMap<String, Vec<(Version, Map<String, Value>)>>;
type Rules = BTreeMap<String, Vec<(Option<String>, Value)>>;

// @yarnpkg/extensions 2.0.1, 2.0.3, 2.0.6 and 2.0.7, plus the rules in
// pnpm's createReadPackageHook and compat_package_extensions.rs. See compat/LICENSE.
static DATABASE: LazyLock<Editions> = LazyLock::new(|| {
    serde_json::from_str(include_str!("compat/extensions.json"))
        .expect("built-in package extensions")
});

#[derive(Default)]
pub(super) struct Extensions {
    dependencies: Rules,
    projects: Option<Rules>,
    database: bool,
    pnpm_version: Option<Version>,
}

/// The database fields for one package under the edition this pnpm release ships.
fn edition(name: &str, pnpm: Option<&Version>) -> Option<&'static Map<String, Value>> {
    DATABASE
        .get(name)?
        .iter()
        .rev()
        .find(|(since, _)| pnpm.is_none_or(|v| v >= since))
        .map(|(_, fields)| fields)
}

impl Extensions {
    pub fn new(config: &WorkspaceConfig, ignore_database: bool) -> Self {
        let version = config.pnpm_version.as_ref();
        let mut dependencies = Rules::new();
        if !ignore_database {
            for name in DATABASE.keys() {
                if let Some(fields) = edition(name, version) {
                    dependencies.insert(
                        name.clone(),
                        fields
                            .iter()
                            .map(|(range, extension)| (Some(range.clone()), extension.clone()))
                            .collect(),
                    );
                }
            }
        }
        let separate_projects = version.is_none_or(|v| v >= &Version::new(11, 28, 1));
        let merge_selectors = version.is_none_or(|v| v >= &Version::new(11, 7, 0));
        let mut projects = separate_projects.then(Rules::new);
        for (selector, extension) in config
            .resolution
            .package_extensions
            .as_ref()
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let (name, range) = manifest::selector(selector);
            let rule = (range.map(str::to_owned), extension.clone());
            if let Some(projects) = &mut projects {
                projects
                    .entry(name.to_owned())
                    .or_default()
                    .push(rule.clone());
            }
            let rules = dependencies.entry(name.to_owned()).or_default();
            if merge_selectors
                && let Some((_, existing)) = rules.iter_mut().find(|(r, _)| r == &rule.0)
            {
                for (group, fields) in extension.as_object().into_iter().flatten() {
                    if let Some(additions) = fields.as_object() {
                        let entries = existing
                            .as_object_mut()
                            .expect("built-in extension")
                            .entry(group)
                            .or_insert_with(|| Value::Object(Map::new()));
                        entries
                            .as_object_mut()
                            .expect("built-in dependency group")
                            .extend(additions.clone());
                    }
                }
            } else {
                rules.push(rule);
            }
        }
        Self {
            dependencies,
            projects,
            database: !ignore_database,
            pnpm_version: version.cloned(),
        }
    }

    /// pnpm applies the database to every manifest it resolves, and the lockfile
    /// keeps no record of `ignoreCompatibilityDb`. A locked release disagrees with
    /// the current setting when it lacks a declaration the database adds, or keeps
    /// one while the database is off. The second case can also be the release's
    /// own declaration; re-reading its manifest settles that.
    pub fn disagrees<'v>(
        &self,
        name: &str,
        version: &str,
        record: &Value,
        snapshots: impl Iterator<Item = &'v Value> + Clone,
    ) -> bool {
        let Some(fields) = edition(name, self.pnpm_version.as_ref()) else {
            return false;
        };
        let recorded = |group: &str, dependency: &str| match group {
            "peerDependencies" | "peerDependenciesMeta" => record[group].get(dependency).is_some(),
            _ => snapshots
                .clone()
                .any(|body| graph::dependency(body, dependency).is_some()),
        };
        fields
            .iter()
            .filter(|(range, _)| manifest::matches_version(range, Some(version)))
            .flat_map(|(_, extension)| extension.as_object().into_iter().flatten())
            .flat_map(|(group, entries)| {
                entries
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(move |(dependency, _)| (group, dependency))
            })
            .any(|(group, dependency)| recorded(group, dependency) != self.database)
    }

    pub fn manifest<'a>(&self, raw: &'a Value, project: bool) -> Result<Cow<'a, Value>> {
        let rules = if project {
            self.projects.as_ref().unwrap_or(&self.dependencies)
        } else {
            &self.dependencies
        };
        let mut result = Cow::Borrowed(raw);
        if let Some(entries) = raw["name"].as_str().and_then(|name| rules.get(name)) {
            for (range, extension) in entries {
                if range
                    .as_ref()
                    .is_none_or(|r| manifest::matches_version(r, raw["version"].as_str()))
                {
                    reconcile::published::add_extension(&mut result, extension)?;
                }
            }
        }
        Ok(result)
    }
}
