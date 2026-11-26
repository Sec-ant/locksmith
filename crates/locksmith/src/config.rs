//! Explicit workspace inputs and the pnpm configuration decoder.
use crate::{LockfileError, Value};
use std::collections::BTreeMap;

/// Which declaration wins when matching override ranges have equal priority.
/// pnpm 9–11 prefer the last matching declaration; pnpm 12 prefers the first.
/// Callers select this explicitly; graph operations do not infer a pnpm version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverrideTieBreak {
    /// Prefer the first matching declaration at equal specificity.
    First,
    /// Prefer the last matching declaration at equal specificity.
    #[default]
    Last,
}

/// Encoding of linked workspace paths inside peer instance identifiers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LinkedPeerEncoding {
    /// pnpm 9–10: filename normalization with a 100 UTF-16 code-unit limit.
    #[default]
    Filename,
    /// pnpm 11–12: normalized path tokens without truncation.
    Path,
}

/// Hash and record format for normalized patch contents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PatchEncoding {
    /// pnpm 9: MD5/Base32 hash and configured path.
    Md5Base32WithPath,
    /// pnpm 10: SHA-256 hash and configured path.
    #[default]
    Sha256WithPath,
    /// pnpm 11 and later: SHA-256 hash without the path.
    Sha256,
}

/// Package-extension checksum written by the consuming package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionChecksum {
    /// MD5 checksum used by pnpm 9.
    Md5,
    /// SHA-256 checksum used by pnpm 10 and later.
    Sha256,
}

/// Which installed packages may supply a newly required optional peer.
/// Adapters choose this explicitly when package-manager versions differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OptionalPeerScope {
    /// Providers installed by any workspace member can satisfy an optional peer.
    #[default]
    Workspace,
    /// Providers must be installed by the consuming project or an eligible workspace root.
    Project,
}

/// Settings that affect the meaning of importer dependency declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockSettings {
    /// Include automatically installed required peers in importer declarations.
    pub auto_install_peers: bool,
    /// Use the compact peer-reference encoding selected by pnpm.
    pub dedupe_peers: bool,
    /// Restrict optional peer providers to the consuming project when peer dependents are deduplicated.
    pub project_scoped_optional_peers: bool,
    /// Omit direct `link:` declarations from importer records.
    pub exclude_links_from_lockfile: bool,
}
impl Default for LockSettings {
    fn default() -> Self {
        Self {
            auto_install_peers: true,
            dedupe_peers: false,
            project_scoped_optional_peers: false,
            exclude_links_from_lockfile: false,
        }
    }
}
impl LockSettings {
    pub(crate) fn from_document(
        document: &Value,
        version: Option<&node_semver::Version>,
    ) -> Result<Self, LockfileError> {
        let Some(settings) = document.get("settings") else {
            return Ok(Self::default());
        };
        let settings = settings
            .as_object()
            .ok_or_else(|| LockfileError::InvalidDocument("settings must be a mapping".into()))?;
        let quoted = version.is_none_or(|v| v.major() >= 12);
        let boolean = |name: &str, default| match settings.get(name) {
            None => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            Some(Value::String(value)) if quoted && value == "true" => Ok(true),
            Some(Value::String(value)) if quoted && value == "false" => Ok(false),
            Some(_) => Err(LockfileError::InvalidDocument(format!(
                "settings/{name} must be a boolean"
            ))),
        };
        Ok(Self {
            auto_install_peers: boolean("autoInstallPeers", true)?,
            dedupe_peers: boolean("dedupePeers", false)?,
            project_scoped_optional_peers: boolean("projectScopedOptionalPeers", false)?,
            exclude_links_from_lockfile: boolean("excludeLinksFromLockfile", false)?,
        })
    }
}

/// Settings used to decide whether locked records can be reused. Package
/// extensions apply to matching names and versions; the other settings and
/// additional parameters are equality constraints on the source graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionSettings {
    /// Dependency settings represented in the lockfile header.
    pub lock: LockSettings,
    /// Allow the workspace root to provide peers for member projects.
    pub workspace_root_peers: bool,
    /// Allow compatible dependents to share peer instances across projects.
    pub dedupe_peer_dependents: bool,
    /// Where to look for already-installed optional peer providers.
    pub optional_peer_scope: OptionalPeerScope,
    /// Materialize workspace dependencies as directory packages by default.
    pub inject_workspace_packages: bool,
    /// Package selectors mapped to dependency and peer metadata additions.
    pub package_extensions: Option<Value>,
    /// Ordered package-name patterns for excluding optional dependencies.
    pub ignored_optional_dependencies: Option<Value>,
    /// Hook identity used to decide whether source records can be reused.
    pub pnpmfile_checksum: Option<Value>,
    /// Additional pnpm resolver settings keyed by their pnpm configuration names.
    pub parameters: BTreeMap<String, Value>,
}
impl Default for ResolutionSettings {
    fn default() -> Self {
        Self {
            lock: LockSettings::default(),
            workspace_root_peers: true,
            dedupe_peer_dependents: true,
            optional_peer_scope: OptionalPeerScope::Workspace,
            inject_workspace_packages: false,
            package_extensions: None,
            ignored_optional_dependencies: None,
            pnpmfile_checksum: None,
            parameters: BTreeMap::new(),
        }
    }
}

/// Normalized workspace data. Callers can supply explicit conventions or
/// decode settings for a particular pnpm release with `from_pnpm_version`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// Consolidate compatible locked releases when the workspace enables pnpm auto-deduplication.
    pub auto_dedupe: bool,
    /// Settings used to interpret importers, snapshots and source compatibility.
    pub resolution: ResolutionSettings,
    /// Configuration dependencies installed from pnpm's bootstrap lock document.
    pub config_dependencies: BTreeMap<String, Value>,
    /// Registry URLs by scope (or `default`) used to reconstruct omitted configuration tarball URLs.
    pub config_registries: BTreeMap<String, String>,
    /// Catalog name to package declarations, with `default` for the unnamed catalog.
    pub catalogs: BTreeMap<String, BTreeMap<String, String>>,
    /// Selector/replacement pairs in declaration order (ties can depend on it).
    pub overrides: Option<Vec<(String, String)>>,
    /// Rule precedence when two override selectors match equally well.
    pub override_tie_break: OverrideTieBreak,
    /// pnpm release responsible for this resolution, when known.
    pub pnpm_version: Option<node_semver::Version>,
    /// Workspace override for the package manager declaration's `onFail` policy.
    pub pm_on_fail: Option<String>,
    /// Path representation to use in peer snapshot identifiers.
    pub linked_peer_encoding: LinkedPeerEncoding,
    /// Declared package selectors mapped to workspace-relative patch file paths.
    pub patches: Option<BTreeMap<String, String>>,
    /// Hash algorithm and record shape for patches in this workspace.
    pub patch_encoding: PatchEncoding,
    /// Checksum format for package extensions, when the package manager is known.
    pub extension_checksum: Option<ExtensionChecksum>,
}
pub(crate) fn locked_config_resolution<'a>(
    env: &'a Value,
    name: &str,
    pin: &Value,
    version: &str,
    integrity: &str,
) -> Option<&'a Value> {
    let resolution = env["packages"]
        .get(format!("{name}@{version}"))?
        .get("resolution")?;
    (integrity.starts_with("sha")
        && version.parse::<node_semver::Version>().is_ok()
        && pin["specifier"] == version
        && pin["version"] == version
        && resolution["integrity"] == integrity)
        .then_some(resolution)
}

impl WorkspaceConfig {
    /// Decode an API context's effective settings. An explicit `packageManager` option
    /// selects the runner. Otherwise use the manifest declaration and the bootstrap lock.
    ///
    /// # Errors
    /// Returns an error for an invalid runner override or invalid pnpm settings.
    pub fn from_pnpm_context(
        raw: &Value,
        manifest: Option<&Value>,
        recorded: Option<node_semver::Version>,
    ) -> Result<Self, LockfileError> {
        let version = match raw.get("packageManager") {
            Some(manager) => Some(
                manager
                    .as_str()
                    .and_then(|s| s.strip_prefix("pnpm@"))
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| {
                        LockfileError::InvalidDocument(
                            "packageManager must be an exact pnpm@version".into(),
                        )
                    })?,
            ),
            None => match manifest {
                Some(root) => Self::project_pnpm_version(root, recorded),
                None => recorded,
            },
        };
        match version {
            Some(version) => Self::from_pnpm_version(raw, &version),
            None => Self::from_pnpm(raw),
        }
    }

    /// Decode a merge or repair source when only the final manifest is available.
    /// A source setting can override the runner; otherwise the source's bootstrap version
    /// takes precedence over the final manifest.
    ///
    /// # Errors
    /// Returns the configuration errors described by [`Self::from_pnpm_context`].
    pub fn from_pnpm_source(
        raw: &Value,
        final_manifest: Option<&Value>,
        recorded: Option<node_semver::Version>,
    ) -> Result<Self, LockfileError> {
        let fallback = if recorded.is_none() {
            final_manifest
        } else {
            None
        };
        Self::from_pnpm_context(raw, fallback, recorded)
    }

    /// Read the project's pnpm version, using its locked version for a compatible range
    /// or when the manifest leaves the version unspecified.
    pub fn project_pnpm_version(
        manifest: &Value,
        recorded: Option<node_semver::Version>,
    ) -> Option<node_semver::Version> {
        let modern_runner = recorded.as_ref().filter(|v| v.major() >= 11);
        let Some(manager) = crate::manifest::PnpmManager::declared(manifest, modern_runner) else {
            return recorded;
        };
        // A range describes the desired runner. The lock records which runner produced these settings.
        manager.specifier.parse().ok().or(recorded)
    }

    /// Decode workspace settings using the lockfile conventions of this pnpm release.
    ///
    /// # Errors
    /// Returns the configuration decoding errors described by [`Self::from_pnpm`].
    pub fn from_pnpm_version(
        value: &Value,
        v: &node_semver::Version,
    ) -> Result<Self, LockfileError> {
        let mut config = Self::from_pnpm(value)?;
        config.pnpm_version = Some(v.clone());
        config.extension_checksum = Some(if v.major() < 10 {
            ExtensionChecksum::Md5
        } else {
            ExtensionChecksum::Sha256
        });
        config.override_tie_break = if v.major() >= 12 {
            OverrideTieBreak::First
        } else {
            OverrideTieBreak::Last
        };
        config.patch_encoding = match v.major() {
            0..=9 => PatchEncoding::Md5Base32WithPath,
            10 => PatchEncoding::Sha256WithPath,
            _ => PatchEncoding::Sha256,
        };
        config.linked_peer_encoding = if v.major() >= 11 {
            LinkedPeerEncoding::Path
        } else {
            LinkedPeerEncoding::Filename
        };
        if !config.since((11, 28, 12), (12, 19, 0)) {
            config.resolution.optional_peer_scope = OptionalPeerScope::Workspace;
        }
        Ok(config)
    }

    pub(crate) fn matches_config_dependencies(&self, environment: Option<&Value>) -> bool {
        // pnpm 9/10 keep configuration packages outside the project lock.
        if environment.is_none() && self.pnpm_version.as_ref().is_some_and(|v| v.major() < 11) {
            return true;
        }
        let recorded = environment.and_then(|env| env["importers"]["."].get("configDependencies"));
        let entries = recorded.and_then(Value::as_object);
        if recorded.is_some() && entries.is_none()
            || entries.map_or(0, |deps| deps.len()) != self.config_dependencies.len()
        {
            return false;
        }
        self.config_dependencies.iter().all(|(name, declaration)| {
            let Some(pin) = entries.and_then(|deps| deps.get(name)) else {
                return false;
            };
            let Some(spec) = declaration
                .as_str()
                .or_else(|| declaration.get("integrity")?.as_str())
            else {
                return false;
            };
            if let Some((version, integrity)) = spec
                .split_once('+')
                .filter(|(_, hash)| hash.starts_with("sha"))
            {
                let Some(resolution) = locked_config_resolution(
                    environment.expect("recorded bootstrap dependency"),
                    name,
                    pin,
                    version,
                    integrity,
                ) else {
                    return false;
                };
                declaration.get("tarball").is_none_or(|url| {
                    match resolution.get("tarball") {
                        Some(locked) => locked == url,
                        None => {
                            // pnpm omits a registry's conventional tarball URL, even when explicitly configured.
                            let bare = name.rsplit('/').next().unwrap();
                            let base = name
                                .split_once('/')
                                .and_then(|(scope, _)| self.config_registries.get(scope))
                                .or_else(|| self.config_registries.get("default"))
                                .map_or("https://registry.npmjs.org", String::as_str);
                            url.as_str()
                                == Some(&format!(
                                    "{}/{name}/-/{bare}-{version}.tgz",
                                    base.trim_end_matches('/')
                                ))
                        }
                    }
                })
            } else {
                declaration.is_string()
                    && pin["specifier"] == spec
                    && pin["version"]
                        .as_str()
                        .and_then(|v| v.parse::<node_semver::Version>().ok())
                        .is_some_and(|version| {
                            spec.parse::<node_semver::Range>().map_or(true, |range| {
                                version.satisfies_with_prerelease(&range, true)
                            })
                        })
            }
        })
    }

    /// Some frozen-lockfile checks shipped separately on the pnpm 11 and 12 lines.
    pub(crate) fn since(&self, pnpm11: (u64, u64, u64), pnpm12: (u64, u64, u64)) -> bool {
        self.pnpm_version.as_ref().is_some_and(|v| match v.major() {
            11 => v >= &pnpm11.into(),
            12 => v >= &pnpm12.into(),
            major => major > 12,
        })
    }

    fn supported_lock_settings(&self, mut settings: LockSettings) -> LockSettings {
        if self
            .pnpm_version
            .as_ref()
            .is_some_and(|v| v < &node_semver::Version::from((12, 19, 0)))
        {
            settings.project_scoped_optional_peers = false;
        }
        settings
    }

    pub(crate) fn read_lock_settings(
        &self,
        document: &Value,
    ) -> Result<LockSettings, LockfileError> {
        Ok(self.supported_lock_settings(LockSettings::from_document(
            document,
            self.pnpm_version.as_ref(),
        )?))
    }

    pub(crate) fn recorded_settings(&self) -> LockSettings {
        let mut settings = self.supported_lock_settings(self.resolution.lock.clone());
        settings.project_scoped_optional_peers &= self.resolution.dedupe_peer_dependents;
        settings
    }

    pub(crate) fn matches_optional_header_for_repair(
        &self,
        document: &Value,
    ) -> Result<bool, LockfileError> {
        let recorded = optional_patterns(document.get("ignoredOptionalDependencies"), true)?;
        let rules = self.resolution.ignored_optional_dependencies.as_ref();
        let mut configured = optional_patterns(rules, true)?;
        if recorded == configured {
            return Ok(true);
        }
        // pnpm 9–11 also write sorted headers for mixed positive and negative rules.
        if self
            .pnpm_version
            .as_ref()
            .is_some_and(|v| (9..=11).contains(&v.major()))
        {
            configured.sort_unstable();
            return Ok(recorded == configured);
        }
        Ok(false)
    }

    pub(crate) fn matches_catalog_ranges(&self) -> bool {
        self.since((11, 28, 12), (12, 20, 0))
    }

    pub(crate) fn supports_omitted_workspace_link(&self, catalog: bool) -> bool {
        self.since((11, 28, 11), (12, if catalog { 18 } else { 6 }, 0))
    }

    pub(crate) fn records_link_directory(&self) -> bool {
        self.since((11, 26, 0), (12, 3, 2))
    }

    /// Whether both configurations name pnpm releases in different major versions.
    pub fn changes_pnpm_major(&self, other: &Self) -> bool {
        self.pnpm_version
            .as_ref()
            .zip(other.pnpm_version.as_ref())
            .is_some_and(|(a, b)| a.major() != b.major())
    }

    /// Whether convergence must reconcile a changed, newly specified, or removed runner version.
    pub fn changes_pnpm_version(&self, other: &Self) -> bool {
        self.pnpm_version != other.pnpm_version
    }

    /// Whether convergence needs to reconcile the installed bootstrap graph and patch records.
    pub fn requires_bootstrap_reconciliation(
        &self,
        after: &Self,
        baseline: &crate::Lockfile,
        candidate: &crate::Lockfile,
    ) -> bool {
        self.changes_pnpm_version(after)
            || self.config_dependencies != after.config_dependencies
            || baseline.recorded_pnpm_version() != candidate.recorded_pnpm_version()
            || !after.matches_config_dependencies(baseline.environment())
    }

    pub(crate) fn matches_pnpm(&self, target: &Self) -> bool {
        target.pnpm_version.is_none() || self.pnpm_version == target.pnpm_version
    }

    /// Decode one effective pnpm configuration object. Files, configuration
    /// precedence, and workspace-glob expansion belong to the caller.
    ///
    /// # Errors
    /// Returns an error when a recognized setting has an invalid type or declaration.
    pub fn from_pnpm(value: &Value) -> Result<Self, LockfileError> {
        if !value.is_object() {
            return Err(LockfileError::InvalidDocument(
                "pnpm configuration must be an object".into(),
            ));
        }
        let mut catalogs: BTreeMap<String, BTreeMap<String, String>> =
            decode(value, "catalogs")?.unwrap_or_default();
        if let Some(default) = decode(value, "catalog")? {
            catalogs.insert("default".into(), default);
        }
        let mut config_registries = BTreeMap::new();
        if let Some(url) = value["registry"].as_str() {
            config_registries.insert("default".into(), url.to_owned());
        }
        for (key, entry) in value["registries"].as_object().into_iter().flatten() {
            if let Some(url) = entry.as_str() {
                config_registries.insert(key.clone(), url.to_owned());
            } else if let Some(scopes) = entry["scopes"].as_array() {
                for scope in scopes.iter().filter_map(Value::as_str) {
                    let scope = if scope == "@" { "default" } else { scope };
                    config_registries.insert(scope.to_owned(), key.clone());
                }
            }
        }
        let dedupe_peer_dependents = decode(value, "dedupePeerDependents")?.unwrap_or(true);
        let project_scoped_optional_peers =
            decode(value, "projectScopedOptionalPeers")?.unwrap_or(false);
        Ok(Self {
            auto_dedupe: decode(value, "autoDedupe")?.unwrap_or(false),
            resolution: ResolutionSettings {
                lock: LockSettings {
                    auto_install_peers: decode(value, "autoInstallPeers")?.unwrap_or(true),
                    dedupe_peers: decode(value, "dedupePeers")?.unwrap_or(false),
                    project_scoped_optional_peers,
                    exclude_links_from_lockfile: decode(value, "excludeLinksFromLockfile")?
                        .unwrap_or(false),
                },
                workspace_root_peers: decode(value, "resolvePeersFromWorkspaceRoot")?
                    .unwrap_or(true),
                dedupe_peer_dependents,
                optional_peer_scope: if dedupe_peer_dependents && !project_scoped_optional_peers {
                    OptionalPeerScope::Workspace
                } else {
                    OptionalPeerScope::Project
                },
                inject_workspace_packages: decode(value, "injectWorkspacePackages")?
                    .unwrap_or(false),
                package_extensions: value.get("packageExtensions").cloned(),
                ignored_optional_dependencies: {
                    let patterns =
                        optional_patterns(value.get("ignoredOptionalDependencies"), true)?;
                    (!patterns.is_empty()).then(|| Value::from(patterns))
                },
                pnpmfile_checksum: value.get("pnpmfileChecksum").cloned(),
                parameters: [
                    "peersSuffixMaxLength",
                    "addMissingPeerTypes",
                    "linkWorkspacePackages",
                    "preferWorkspacePackages",
                ]
                .into_iter()
                .filter_map(|field| value.get(field).map(|v| (field.into(), v.clone())))
                .collect(),
            },
            catalogs,
            config_registries,
            config_dependencies: decode(value, "configDependencies")?.unwrap_or_default(),
            overrides: string_rules(value, "overrides")?,
            override_tie_break: OverrideTieBreak::default(),
            pnpm_version: None,
            pm_on_fail: decode(value, "pmOnFail")?,
            linked_peer_encoding: LinkedPeerEncoding::default(),
            patches: decode::<Option<_>>(value, "patchedDependencies")?.flatten(),
            patch_encoding: PatchEncoding::default(),
            extension_checksum: None,
        })
    }
}

/// Order matters only for mixed positive and negative patterns. Serialized
/// pnpm headers can be sorted even when the configured matcher uses authored order.
pub(crate) fn optional_patterns(
    value: Option<&Value>,
    preserve_mixed_order: bool,
) -> Result<Vec<&str>, LockfileError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let invalid = || {
        LockfileError::InvalidDocument(
            "ignoredOptionalDependencies must be an array of strings".into(),
        )
    };
    let mut names: Vec<_> = value
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|v| v.as_str().ok_or_else(invalid))
        .collect::<Result<_, _>>()?;
    if !preserve_mixed_order
        || names.iter().all(|n| n.starts_with('!'))
        || names.iter().all(|n| !n.starts_with('!'))
    {
        names.sort_unstable();
    }
    Ok(names)
}

impl ResolutionSettings {
    pub(crate) fn ignored_optional_dependencies_match(
        &self,
        document: &Value,
    ) -> Result<bool, LockfileError> {
        Ok(
            optional_patterns(document.get("ignoredOptionalDependencies"), false)?
                == optional_patterns(self.ignored_optional_dependencies.as_ref(), false)?,
        )
    }
}

// pnpm treats only `*` as a wildcard. The last matching rule wins;
// a list of only negative rules includes every name they do not match.
pub(crate) fn matches_patterns<'a>(
    patterns: impl DoubleEndedIterator<Item = &'a str>,
    name: &str,
) -> bool {
    let mut patterns = patterns.rev().peekable();
    let mut only_negative = patterns.peek().is_some();
    for pattern in patterns {
        let negative = pattern.strip_prefix('!');
        if wildcard(negative.unwrap_or(pattern), name) {
            return negative.is_none();
        }
        only_negative &= negative.is_some();
    }
    only_negative
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().expect("one split part");
    let Some(mut rest) = value.strip_prefix(first) else {
        return false;
    };
    let mut parts = parts.peekable();
    if parts.peek().is_none() {
        return rest.is_empty();
    }
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        let Some(offset) = rest.find(part) else {
            return false;
        };
        rest = &rest[offset + part.len()..];
    }
    true
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(
    value: &Value,
    field: &str,
) -> Result<Option<T>, LockfileError> {
    value
        .get(field)
        .map(|v| {
            serde_json::from_value(v.clone()).map_err(|e| {
                LockfileError::InvalidDocument(format!("pnpm configuration {field}: {e}"))
            })
        })
        .transpose()
}

pub(crate) fn string_rules(
    value: &Value,
    field: &str,
) -> Result<Option<Vec<(String, String)>>, LockfileError> {
    value
        .get(field)
        .map(|value| {
            let object = value.as_object().ok_or_else(|| {
                LockfileError::InvalidDocument(format!(
                    "pnpm configuration {field} must be a mapping"
                ))
            })?;
            object
                .iter()
                .map(|(selector, value)| {
                    value
                        .as_str()
                        .map(|v| (selector.clone(), v.into()))
                        .ok_or_else(|| {
                            LockfileError::InvalidDocument(format!(
                                "pnpm configuration {field}.{selector} must be a string"
                            ))
                        })
                })
                .collect()
        })
        .transpose()
}

pub(crate) const EXTENSION_GROUPS: [&str; 4] = [
    "dependencies",
    "optionalDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
];
type ExtensionFields<'a> = [BTreeMap<&'a str, &'a Value>; 4];
type EffectiveExtensions<'a> = [ExtensionFields<'a>; 2];
type ExtensionRules<'a> = Vec<(&'a str, &'a Value)>;

pub(crate) fn extension_fields<'a>(
    rules: &[(&'a str, &'a Value)],
    name: &str,
    version: &str,
) -> Option<ExtensionFields<'a>> {
    let mut fields: ExtensionFields<'a> = Default::default();
    for (selector, extension) in rules {
        if crate::manifest::parent_matches(selector, name, version) {
            for (i, group) in EXTENSION_GROUPS.iter().enumerate() {
                if let Some(values) = extension.get(*group) {
                    for (dep, value) in values.as_object()? {
                        fields[i].entry(dep).or_insert(value);
                    }
                }
            }
        }
    }
    Some(fields)
}

pub(crate) struct ExtensionAddition<'a> {
    pub group: &'static str,
    pub name: &'a str,
    pub specifier: &'a str,
}

/// Compare global settings once and index changed extensions by package name.
/// An extension for one package does not invalidate unrelated locked packages.
pub(crate) struct ResolutionCompatibility<'a> {
    global: bool,
    extensions: BTreeMap<&'a str, [ExtensionRules<'a>; 2]>,
    additions: BTreeMap<&'a str, Vec<ExtensionAddition<'a>>>,
}

impl<'a> ResolutionCompatibility<'a> {
    pub(crate) fn new(
        source: &'a ResolutionSettings,
        target: &'a ResolutionSettings,
        document: &'a Value,
    ) -> Self {
        let global = source.lock == target.lock
            && source.dedupe_peer_dependents == target.dedupe_peer_dependents
            && source.optional_peer_scope == target.optional_peer_scope
            && source.workspace_root_peers == target.workspace_root_peers
            && source.inject_workspace_packages == target.inject_workspace_packages
            && source.ignored_optional_dependencies == target.ignored_optional_dependencies
            && source.pnpmfile_checksum == target.pnpmfile_checksum
            && source
                .parameters
                .iter()
                .filter(|(k, v)| k.as_str() != "linkWorkspacePackages" || *v == "deep")
                .eq(target
                    .parameters
                    .iter()
                    .filter(|(k, v)| k.as_str() != "linkWorkspacePackages" || *v == "deep"));
        let mut result = Self {
            global,
            extensions: BTreeMap::new(),
            additions: BTreeMap::new(),
        };
        for (side, extensions) in [&source.package_extensions, &target.package_extensions]
            .into_iter()
            .enumerate()
        {
            let Some(extensions) = extensions else {
                continue;
            };
            let Some(extensions) = extensions.as_object() else {
                result.global = false;
                continue;
            };
            for (selector, extension) in extensions {
                let (_, name, _) = crate::manifest::override_selector(selector);
                result.extensions.entry(name).or_default()[side].push((selector, extension));
            }
        }
        result.extensions.retain(|_, rules| rules[0] != rules[1]);
        if result.global && !result.extensions.is_empty() {
            for (key, _) in crate::graph::section(document, "packages")
                .into_iter()
                .flatten()
            {
                if !result.unchanged(key)
                    && let Some(additions) = result.additional_dependencies(key)
                    && (additions.is_empty()
                        || (source.pnpmfile_checksum.is_none()
                            && document.get("pnpmfileChecksum").is_none()))
                {
                    result.additions.insert(key, additions);
                }
            }
        }
        result
    }

    pub(crate) fn accepts(&self, key: &str) -> bool {
        self.unchanged(key) || self.additions.contains_key(crate::graph::package_key(key))
    }

    pub(crate) fn additions(&self, key: &str) -> &[ExtensionAddition<'a>] {
        self.additions
            .get(crate::graph::package_key(key))
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn effective_extensions(&self, key: &str) -> Option<EffectiveExtensions<'a>> {
        if !self.global {
            return None;
        }
        let (name, version) = crate::graph::package_key(key).rsplit_once('@')?;
        let rules = self.extensions.get(name)?;
        Some([
            extension_fields(&rules[0], name, version)?,
            extension_fields(&rules[1], name, version)?,
        ])
    }

    fn additional_dependencies(&self, key: &str) -> Option<Vec<ExtensionAddition<'a>>> {
        let effective = self.effective_extensions(key)?;
        if effective[0][2..] != effective[1][2..] {
            return None;
        }
        let mut additions = Vec::new();
        for (field, group) in EXTENSION_GROUPS[..2].iter().enumerate() {
            let before = &effective[0][field];
            let after = &effective[1][field];
            if before
                .iter()
                .any(|(name, value)| after.get(name) != Some(value))
            {
                return None;
            }
            for (name, value) in after {
                if !before.contains_key(name) {
                    additions.push(ExtensionAddition {
                        group,
                        name,
                        specifier: value.as_str()?,
                    });
                }
            }
        }
        Some(additions)
    }

    pub(crate) fn unchanged(&self, key: &str) -> bool {
        if !self.global {
            return false;
        }
        if self.extensions.is_empty() || key.starts_with("link:") {
            return true;
        }
        let Some((name, version)) = crate::graph::package_key(key).rsplit_once('@') else {
            return false;
        };
        let Some(rules) = self.extensions.get(name) else {
            return true;
        };
        // Preserve declaration order: pnpm applies the first extension's field
        // values before later matching extensions fill in missing fields.
        let applicable =
            |rule: &&(&str, &Value)| crate::manifest::parent_matches(rule.0, name, version);
        rules[0]
            .iter()
            .filter(applicable)
            .map(|(_, value)| value)
            .eq(rules[1].iter().filter(applicable).map(|(_, value)| value))
    }
}
