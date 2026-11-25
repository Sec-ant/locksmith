use std::{borrow::Cow, collections::BTreeMap};

use crate::{LockfileError, OverrideTieBreak, Value, WorkspaceConfig, graph};

pub(crate) const GROUPS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];
pub(crate) type Declarations<'a> = BTreeMap<&'a str, (&'static str, Cow<'a, str>)>;

pub(crate) fn alias(specifier: &str) -> Option<(&str, &str)> {
    let alias = specifier.strip_prefix("npm:")?;
    Some(
        alias
            .rsplit_once('@')
            .filter(|(name, _)| !name.is_empty())
            .unwrap_or((alias, "*")),
    )
}

/// A bare npm: range keeps the dependency key as its target package.
pub(crate) fn registry_request<'a>(name: &'a str, specifier: &'a str) -> (&'a str, &'a str) {
    if let Some(range) = specifier.strip_prefix("npm:")
        && range.parse::<node_semver::Range>().is_ok()
    {
        (name, range)
    } else {
        alias(specifier).unwrap_or((name, specifier))
    }
}

/// pnpm omits rule maps with no entries when writing the lockfile.
pub(crate) fn rule_map<'a>(document: &'a Value, field: &str) -> Option<&'a Value> {
    document
        .get(field)
        .filter(|value| !value.as_object().is_some_and(|rules| rules.is_empty()))
}

pub(crate) fn same_overrides(left: &Value, right: &Value) -> bool {
    let config = WorkspaceConfig::default();
    rule_map(left, "overrides") == rule_map(right, "overrides")
        && Overrides::new(left, &config).same_rules(&Overrides::new(right, &config))
}

pub(crate) fn same_override_order(left: &Value, right: &Value) -> bool {
    same_ordered_mapping(left.get("overrides"), right.get("overrides"))
}

pub(crate) fn same_ordered_mapping(left: Option<&Value>, right: Option<&Value>) -> bool {
    left == right
        && left
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .eq(right.and_then(Value::as_object).into_iter().flatten())
}

/// pnpm precedence: optional > production > dev > auto-installed peers.
pub(crate) fn declarations<'a>(
    manifest: &'a Value,
    overrides: &Overrides<'a>,
    importer: &str,
) -> Result<Declarations<'a>, LockfileError> {
    let ignores_optional = overrides.optional_exclusion()?;
    // pnpm 12 checks optional exclusions after peers have entered the production group.
    let exclude_peer = overrides
        .config
        .pnpm_version
        .as_ref()
        .is_some_and(|v| v.major() >= 12);
    let mut result = BTreeMap::new();
    let settings = crate::LockSettings::from_document(
        overrides.document,
        overrides.configuration().pnpm_version.as_ref(),
    )?;
    for (field, group) in [
        ("devDependencies", "devDependencies"),
        ("dependencies", "dependencies"),
        ("peerDependencies", "dependencies"),
        ("optionalDependencies", "optionalDependencies"),
    ] {
        let filter_optional = overrides
            .config
            .resolution
            .ignored_optional_dependencies
            .is_some()
            && (matches!(field, "dependencies" | "optionalDependencies")
                || (exclude_peer && field == "peerDependencies"));
        let engines = match field {
            "devDependencies" => manifest.get("devEngines"),
            "dependencies" => manifest.get("engines"),
            _ => None,
        };
        if let Some(runtime) = engines.and_then(|v| v.get("runtime")) {
            let runtimes = runtime
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(runtime));
            for &name in &["node", "deno", "bun"] {
                if manifest.get(field).and_then(|v| v.get(name)).is_some() {
                    continue;
                }
                if let Some(runtime) = runtimes.iter().find(|v| v["name"] == name)
                    && runtime["onFail"] == "download"
                    && let Some(version) = runtime["version"].as_str().filter(|v| !v.is_empty())
                {
                    let specifier = format!("runtime:{version}");
                    match overrides.select(manifest, name, &specifier)? {
                        Some(value) => {
                            if let Some(value) = overrides.resolve(importer, name, value)? {
                                result.insert(name, (group, value));
                            }
                        }
                        None => {
                            result.insert(name, (group, Cow::Owned(specifier)));
                        }
                    }
                }
            }
        }
        let Some(deps) = manifest.get(field) else {
            continue;
        };
        let deps = deps
            .as_object()
            .ok_or_else(|| LockfileError::InvalidManifest(importer.into()))?;
        for (name, specifier) in deps {
            let specifier = specifier
                .as_str()
                .ok_or_else(|| LockfileError::InvalidManifest(importer.into()))?;
            if filter_optional
                && manifest["optionalDependencies"].get(name).is_some()
                && ignores_optional(name)
            {
                continue;
            }
            let replacement = overrides.select(manifest, name, specifier)?;
            let effective = match replacement {
                Some(value) => overrides.resolve(importer, name, value)?,
                None => Some(Cow::Borrowed(specifier)),
            };
            let Some(effective) = effective else { continue };
            if field == "peerDependencies" {
                let promoted = replacement.is_some() && !valid_peer_range(&effective);
                // pnpm 10 through 12.16 promoted a peer overridden to a local
                // link even when the importer already declared it for development.
                let replaces_dev = promoted
                    && overrides
                        .config
                        .pnpm_version
                        .as_ref()
                        .is_none_or(|version| {
                            version.major() >= 10
                                && version < &node_semver::Version::from((12, 17, 0))
                        });
                if (!promoted && !settings.auto_install_peers)
                    || (!replaces_dev && result.contains_key(name.as_str()))
                {
                    continue;
                }
            }
            result.insert(name.as_str(), (group, effective));
        }
    }
    if settings.exclude_links_from_lockfile {
        result.retain(|_, (_, specifier)| !specifier.starts_with("link:"));
    }
    Ok(result)
}

pub(crate) fn valid_peer_range(specifier: &str) -> bool {
    specifier.contains("workspace:")
        || specifier.contains("catalog:")
        || specifier.parse::<node_semver::Range>().is_ok()
}

pub(crate) fn mismatch(
    importer: &str,
    side: &'static str,
    message: impl Into<String>,
) -> LockfileError {
    LockfileError::ManifestMismatch {
        importer: importer.into(),
        side,
        message: message.into(),
    }
}

pub(crate) fn check(
    overrides: &Overrides<'_>,
    importer: &str,
    manifest: Option<&Value>,
    side: &'static str,
) -> Result<(), LockfileError> {
    check_manifest(overrides, importer, manifest, side, ManifestCheck::Authored)
}

pub(crate) enum ManifestCheck<'a> {
    Authored,
    RecordedBaseline,
    Frozen(&'a crate::workspace_links::FrozenWorkspace<'a>),
}

fn recorded_declarations<'a>(
    manifest: &'a Value,
    id: &str,
) -> Result<Declarations<'a>, LockfileError> {
    let mut expected = Declarations::new();
    for group in GROUPS {
        let Some(entries) = manifest.get(group) else {
            continue;
        };
        let entries = entries
            .as_object()
            .ok_or_else(|| LockfileError::InvalidManifest(id.into()))?;
        for (name, specifier) in entries {
            let specifier = specifier
                .as_str()
                .ok_or_else(|| LockfileError::InvalidManifest(id.into()))?;
            if expected
                .insert(name, (group, Cow::Borrowed(specifier)))
                .is_some()
            {
                return Err(mismatch(
                    id,
                    "baseline",
                    format!("duplicate dependency {name}"),
                ));
            }
        }
    }
    Ok(expected)
}

pub(crate) fn check_manifest(
    overrides: &Overrides<'_>,
    importer: &str,
    manifest: Option<&Value>,
    side: &'static str,
    mode: ManifestCheck<'_>,
) -> Result<(), LockfileError> {
    let workspace = match mode {
        ManifestCheck::Frozen(ws) => Some(ws),
        _ => None,
    };
    let document = overrides.document;
    let record = document.get("importers").and_then(|map| map.get(importer));
    let Some(manifest) = manifest else {
        return if record.is_none() {
            Ok(())
        } else {
            Err(mismatch(
                importer,
                side,
                "deleted/absent project still has an importer",
            ))
        };
    };
    if !manifest.is_object() {
        return Err(LockfileError::InvalidManifest(importer.into()));
    }
    let empty;
    let record = match record {
        Some(value) if value.is_object() => value,
        None if workspace.is_some_and(|ws| ws.can_omit_empty_importer(importer)) => {
            empty = Value::Object(Default::default());
            &empty
        }
        _ => return Err(mismatch(importer, side, "missing importer")),
    };
    let mut expected = match mode {
        ManifestCheck::RecordedBaseline => recorded_declarations(manifest, importer)?,
        _ => declarations(manifest, overrides, importer)?,
    };
    let mut actual = BTreeMap::new();
    for group in GROUPS {
        if let Some(deps) = record.get(group) {
            let deps = deps
                .as_object()
                .ok_or_else(|| mismatch(importer, side, format!("{group} must be a mapping")))?;
            for (name, dependency) in deps {
                let specifier = dependency
                    .get("specifier")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        mismatch(importer, side, format!("missing specifier for {name}"))
                    })?;
                let version = dependency
                    .get("version")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        mismatch(importer, side, format!("missing resolution for {name}"))
                    })?;
                if actual
                    .insert(name.as_str(), (group, Cow::Borrowed(specifier)))
                    .is_some()
                {
                    return Err(mismatch(
                        importer,
                        side,
                        format!("duplicate dependency {name}"),
                    ));
                }
                // npm ranges differ from Cargo's semver ranges. Non-registry
                // protocols (workspace, catalog, git, file) are graph-validated.
                if let Some(ws) = workspace
                    && expected
                        .get(name.as_str())
                        .is_some_and(|(g, s)| *g == group && s == specifier)
                {
                    ws.check_pin(document, importer, name, specifier, version)?;
                }
                if version.starts_with("link:") {
                    continue;
                }
                let resolved_key;
                let (range, version) = if specifier.starts_with("npm:") {
                    let (target, range) = registry_request(name, specifier);
                    let prefix = format!("{target}@");
                    resolved_key =
                        graph::snapshot_key(name, version, graph::section(document, "snapshots"));
                    let resolved = graph::package_key(version)
                        .strip_prefix(&prefix)
                        .or_else(|| graph::package_key(&resolved_key).strip_prefix(&prefix))
                        .ok_or_else(|| {
                            mismatch(
                                importer,
                                side,
                                format!("alias {name} does not resolve to {target}"),
                            )
                        })?;
                    (range, resolved)
                } else {
                    (specifier, graph::package_key(version))
                };
                // pnpm accepts the registry's latest tag for *, including prereleases.
                if range != "*"
                    && let Ok(range) = range.parse::<node_semver::Range>()
                    && let Ok(version) = version.parse::<node_semver::Version>()
                    && !version.satisfies(&range)
                {
                    return Err(mismatch(
                        importer,
                        side,
                        format!("resolution of {name} does not satisfy {specifier}"),
                    ));
                }
            }
        }
    }
    // A matching workspace member can explain an omitted pin. An existing
    // registry pin for the same range still has to match its declaration.
    if overrides.config.resolution.lock.exclude_links_from_lockfile
        && let Some(ws) = workspace
    {
        expected
            .retain(|name, (_, spec)| actual.contains_key(name) || !ws.links.can_omit(name, spec));
    }
    if actual != expected {
        let name = expected
            .keys()
            .chain(actual.keys())
            .find(|name| expected.get(**name) != actual.get(**name))
            .expect("different maps have a differing key");
        return Err(mismatch(
            importer,
            side,
            format!(
                "dependency {name}: expected {:?}, found {:?}",
                expected.get(*name),
                actual.get(*name)
            ),
        ));
    }
    let empty = serde_json::json!({});
    if manifest.get("dependenciesMeta").unwrap_or(&empty)
        != record.get("dependenciesMeta").unwrap_or(&empty)
    {
        return Err(mismatch(importer, side, "dependenciesMeta differs"));
    }
    if manifest
        .get("publishConfig")
        .and_then(|v| v.get("directory"))
        != record.get("publishDirectory")
    {
        return Err(mismatch(importer, side, "publishDirectory differs"));
    }
    if overrides.config.records_link_directory()
        && record.get("publishDirectory").is_some()
        && (record["linkDirectory"] == false)
            != (manifest["publishConfig"]["linkDirectory"] == false)
    {
        return Err(mismatch(importer, side, "linkDirectory differs"));
    }
    Ok(())
}

pub(crate) fn link_directory<'a>(
    manifest: &'a Value,
    config: &WorkspaceConfig,
) -> Option<&'a Value> {
    let publish = &manifest["publishConfig"];
    publish.get("linkDirectory").filter(|v| {
        config.records_link_directory()
            && publish.get("directory").is_some()
            && v.as_bool() == Some(false)
    })
}

pub(crate) fn matches_version(range: &str, version: Option<&str>) -> bool {
    let range = range.parse::<node_semver::Range>();
    let version = version.and_then(|s| s.parse::<node_semver::Version>().ok());
    matches!((range, version), (Ok(range), Some(version)) if version.satisfies(&range))
}

pub(crate) fn selector(value: &str) -> (&str, Option<&str>) {
    value
        .char_indices()
        .skip(1)
        .find(|(_, c)| *c == '@')
        .map_or((value, None), |(i, _)| (&value[..i], Some(&value[i + 1..])))
}
fn intersects(selector: Option<&str>, specifier: &str) -> bool {
    let Some(selector) = selector else {
        return true;
    };
    if selector == specifier {
        return true;
    }
    let selector = selector.parse::<node_semver::Range>();
    let declared = specifier.parse::<node_semver::Range>();
    matches!((selector, declared), (Ok(selector), Ok(declared)) if selector.allows_any(&declared))
}

pub(crate) fn override_selector(rule: &str) -> (Option<&str>, &str, Option<&str>) {
    let delimiter = rule
        .char_indices()
        .find(|(i, c)| *c == '>' && *i > 0 && !b" |@".contains(&rule.as_bytes()[i - 1]))
        .map(|(i, _)| i);
    let (parent, target) = delimiter.map_or((None, rule), |i| (Some(&rule[..i]), &rule[i + 1..]));
    let (name, range) = selector(target);
    (parent, name, range)
}

pub(crate) fn override_scope(rule: &str) -> (&str, Option<&str>) {
    let (parent, name, _) = override_selector(rule);
    (name, parent.map(|parent| selector(parent).0))
}

/// Registry overrides whose effective direct specifier is known without a
/// manifest. Qualified rules require the original declaration or parent identity,
/// neither of which is fully represented in an importer.
pub(crate) fn unconditional_overrides(document: &Value) -> BTreeMap<&str, &str> {
    let mut candidates = BTreeMap::new();
    let rules = graph::section(document, "overrides");
    for (rule, value) in rules.into_iter().flatten() {
        let (parent, name, range) = override_selector(rule);
        if parent.is_none()
            && range.is_none()
            && let Some(value) = value.as_str()
            && value.parse::<node_semver::Range>().is_ok()
        {
            candidates.insert(name, value);
        }
    }
    for (rule, _) in rules.into_iter().flatten() {
        let (parent, name, range) = override_selector(rule);
        if parent.is_some() || range.is_some() {
            candidates.remove(name);
        }
    }
    candidates
}

#[derive(PartialEq)]
struct OverrideRule<'a> {
    selector: &'a str,
    parent: Option<&'a str>,
    range: Option<&'a str>,
    value: &'a Value,
}

#[derive(Clone, Copy)]
enum OptionalRules {
    Configured,
    Sorted,
    Disabled,
}

/// Index once per manifest so unrelated selectors are not reparsed for each
/// dependency. Rules for one name retain their declaration order.
pub(crate) struct Overrides<'a> {
    document: &'a Value,
    by_name: BTreeMap<&'a str, Vec<OverrideRule<'a>>>,
    ignored_optional: std::cell::OnceCell<Vec<String>>,
    optional_rules: OptionalRules,
    config: &'a WorkspaceConfig,
}

impl<'a> Overrides<'a> {
    pub(crate) fn configuration(&self) -> &WorkspaceConfig {
        self.config
    }

    pub(crate) fn optional_exclusion(&self) -> Result<impl Fn(&str) -> bool + '_, LockfileError> {
        let patterns = match self.ignored_optional.get() {
            Some(patterns) => patterns,
            None => {
                let raw = self
                    .config
                    .resolution
                    .ignored_optional_dependencies
                    .as_ref();
                let rules = match self.optional_rules {
                    OptionalRules::Disabled => Vec::new(),
                    OptionalRules::Configured => crate::config::optional_patterns(raw, true)?,
                    OptionalRules::Sorted => crate::config::optional_patterns(raw, false)?,
                };
                self.ignored_optional
                    .get_or_init(|| rules.into_iter().map(str::to_owned).collect())
            }
        };
        Ok(move |name: &str| {
            crate::config::matches_patterns(patterns.iter().map(String::as_str), name)
        })
    }

    pub(crate) fn for_frozen_workspace(document: &'a Value, config: &'a WorkspaceConfig) -> Self {
        // pnpm 11.21+ on the 11.x line sorts mixed rules during the frozen check.
        // Fresh resolution uses the configured order, including for snapshot edges.
        let sorted = config
            .pnpm_version
            .as_ref()
            .is_some_and(|v| v.major() == 11 && v >= &node_semver::Version::from((11, 21, 0)));
        Self {
            optional_rules: if sorted {
                OptionalRules::Sorted
            } else {
                OptionalRules::Configured
            },
            ..Self::new(document, config)
        }
    }

    pub(crate) fn relevant(
        &self,
        name: &str,
        parent: &str,
        version: &str,
    ) -> Vec<(&'a str, &'a Value)> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|rule| {
                rule.parent
                    .is_none_or(|s| parent_matches(s, parent, version))
            })
            .map(|rule| (rule.selector, rule.value))
            .collect()
    }

    pub(crate) fn new(document: &'a Value, config: &'a WorkspaceConfig) -> Self {
        let mut by_name: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (rule, value) in graph::section(document, "overrides").into_iter().flatten() {
            let (parent, name, range) = override_selector(rule);
            by_name.entry(name).or_default().push(OverrideRule {
                selector: rule,
                parent,
                range,
                value,
            });
        }
        Self {
            document,
            by_name,
            ignored_optional: Default::default(),
            // pnpm's native resolver began applying optional exclusions to fresh
            // importer and package manifests in 12.4.1. Earlier 12.x frozen
            // checks already used the configured patterns.
            optional_rules: if config
                .pnpm_version
                .as_ref()
                .is_some_and(|v| v.major() == 12 && v < &node_semver::Version::from((12, 4, 1)))
            {
                OptionalRules::Disabled
            } else {
                OptionalRules::Configured
            },
            config,
        }
    }

    pub(crate) fn same_rules(&self, other: &Self) -> bool {
        if self.config.override_tie_break != other.config.override_tie_break
            && self.by_name.values().any(|rules| rules.len() > 1)
        {
            return false;
        }
        self.by_name.len() == other.by_name.len()
            && self.by_name.iter().all(|(name, left)| {
                other.by_name.get(name).is_some_and(|right| {
                    if left == right {
                        return true;
                    }
                    // Different parent packages never compete. Parent-qualified
                    // rules also take precedence over generic rules regardless
                    // of their relative position in the configuration.
                    let mut left: Vec<_> = left.iter().collect();
                    let mut right: Vec<_> = right.iter().collect();
                    left.sort_by_key(|rule| rule.parent.map(|parent| selector(parent).0));
                    right.sort_by_key(|rule| rule.parent.map(|parent| selector(parent).0));
                    left == right
                })
            })
    }

    /// Mirror pnpm's direct-dependency override selection. Parent-qualified rules
    /// take precedence; overlapping rules use pnpm's most-specific selection order.
    pub(crate) fn apply(
        &self,
        manifest: &Value,
        importer: &str,
        name: &str,
        specifier: &'a str,
    ) -> Result<Option<Cow<'a, str>>, LockfileError> {
        match self.select(manifest, name, specifier)? {
            Some(value) => self.resolve(importer, name, value),
            None => Ok(Some(Cow::Borrowed(specifier))),
        }
    }

    pub(crate) fn select(
        &self,
        manifest: &Value,
        name: &str,
        specifier: &str,
    ) -> Result<Option<&'a str>, LockfileError> {
        let Some(rules) = self.by_name.get(name) else {
            return Ok(None);
        };
        let specifier = if self.config.matches_catalog_ranges() {
            specifier
                .strip_prefix("catalog:")
                .and_then(|catalog| self.catalog_specifier(catalog, name))
                .unwrap_or(specifier)
        } else {
            specifier
        };
        let mut generic: Option<(Option<&str>, &str)> = None;
        let mut qualified: Option<(Option<&str>, &str)> = None;
        for rule in rules {
            let OverrideRule {
                selector: rule,
                parent,
                range,
                value,
            } = *rule;
            if let Some(parent) = parent {
                let (parent_name, range) = selector(parent);
                if manifest.get("name").and_then(Value::as_str) != Some(parent_name) {
                    continue;
                }
                if let Some(range) = range
                    && !matches_version(range, manifest["version"].as_str())
                {
                    continue;
                }
            }
            if !intersects(range, specifier) {
                continue;
            }
            let value = value.as_str().ok_or_else(|| {
                LockfileError::InvalidDocument(format!("override {rule:?} must be a string"))
            })?;
            let best = if parent.is_some() {
                &mut qualified
            } else {
                &mut generic
            };
            if best.is_none_or(|(old, _)| {
                intersects(old, range.unwrap_or(""))
                    && (self.config.override_tie_break == OverrideTieBreak::Last
                        || !intersects(range, old.unwrap_or("")))
            }) {
                *best = Some((range, value));
            }
        }
        Ok(qualified.or(generic).map(|(_, value)| value))
    }

    fn catalog_specifier(&self, catalog: &str, name: &str) -> Option<&'a str> {
        let catalog = if catalog.is_empty() {
            "default"
        } else {
            catalog
        };
        self.config
            .catalogs
            .get(catalog)?
            .get(name)
            .map(String::as_str)
    }

    pub(crate) fn resolve(
        &self,
        importer: &str,
        name: &str,
        mut value: &'a str,
    ) -> Result<Option<Cow<'a, str>>, LockfileError> {
        if value == "-" {
            return Ok(None);
        }
        if let Some(catalog) = value.strip_prefix("catalog:") {
            value = self
                .catalog_specifier(catalog, name)
                .or_else(|| {
                    let catalog = if catalog.is_empty() {
                        "default"
                    } else {
                        catalog
                    };
                    self.document["catalogs"][catalog][name]["specifier"].as_str()
                })
                .ok_or_else(|| {
                    LockfileError::InvalidDocument(format!(
                        "override for {name} references a missing catalog entry"
                    ))
                })?;
        }
        for protocol in ["link:", "file:"] {
            if let Some(path) = value.strip_prefix(protocol)
                && !path.starts_with('/')
                && path.as_bytes().get(1) != Some(&b':')
            {
                let path = graph::linked_importer(".", path);
                let from: Vec<_> = importer
                    .split('/')
                    .filter(|s| !s.is_empty() && *s != ".")
                    .collect();
                let to: Vec<_> = path
                    .split('/')
                    .filter(|s| !s.is_empty() && *s != ".")
                    .collect();
                let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
                let relative = std::iter::repeat_n("..", from.len() - common)
                    .chain(to[common..].iter().copied())
                    .collect::<Vec<_>>()
                    .join("/");
                return Ok(Some(Cow::Owned(format!(
                    "{protocol}{}",
                    if relative.is_empty() { "." } else { &relative }
                ))));
            }
        }
        Ok(Some(Cow::Borrowed(value)))
    }
}

pub(crate) fn parent_matches(selector: &str, name: &str, version: &str) -> bool {
    selector == name
        || selector
            .strip_prefix(name)
            .and_then(|s| s.strip_prefix('@'))
            .is_some_and(|r| {
                matches!((r.parse::<node_semver::Range>(), version.parse::<node_semver::Version>()),
            (Ok(range), Ok(version)) if version.satisfies(&range))
            })
}
