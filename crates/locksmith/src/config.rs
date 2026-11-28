//! Explicit workspace inputs and the pnpm configuration decoder.
use crate::{LockfileError, Value};
use std::collections::BTreeMap;

const INCLUDE_RESOLUTION_SETTINGS: &str = "lockfile.includeResolutionSettings";
const INTEGRITY_TARGET: &str = "lockfile.integrityTarget";

mod bootstrap;
mod compatibility;

pub(crate) use bootstrap::locked_config_resolution;
pub(crate) use compatibility::{
    ExtensionAddition, ExtensionFields, ResolutionCompatibility, extension_fields,
    matches_patterns, optional_patterns,
};

/// Which declaration wins when matching override ranges have equal priority.
/// pnpm 9–11 prefer the last matching declaration; pnpm 12 prefers the first.
/// Callers select this explicitly; graph operations do not infer a pnpm version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OverrideTieBreak {
    /// Prefer the first matching declaration at equal specificity.
    First,
    /// Prefer the last matching declaration at equal specificity.
    #[default]
    Last,
}

/// Encoding of linked workspace paths inside peer instance identifiers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LinkedPeerEncoding {
    /// pnpm 9–10: filename normalization with a 100 UTF-16 code-unit limit.
    #[default]
    Filename,
    /// pnpm 11–12: normalized path tokens without truncation.
    Path,
}

/// Hash and record format for normalized patch contents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ExtensionChecksum {
    /// MD5 checksum used by pnpm 9.
    Md5,
    /// SHA-256 checksum used by pnpm 10 and later.
    #[default]
    Sha256,
}

/// Which installed packages may supply a newly required optional peer.
/// Adapters choose this explicitly when package-manager versions differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
/// decode settings for a particular pnpm release with [`Self::from_pnpm_version`].
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
    #[must_use]
    pub fn project_pnpm_version(
        manifest: &Value,
        recorded: Option<node_semver::Version>,
    ) -> Option<node_semver::Version> {
        let modern_runner = recorded.as_ref().filter(|v| v.major() >= 11);
        let Some(manager) = crate::schema::PnpmManager::declared(manifest, modern_runner) else {
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

    /// Some frozen-lockfile checks shipped separately on the pnpm 11 and 12 lines.
    pub(crate) fn since(&self, pnpm11: (u64, u64, u64), pnpm12: (u64, u64, u64)) -> bool {
        self.pnpm_version.as_ref().is_some_and(|v| match v.major() {
            11 => v >= &pnpm11.into(),
            12 => v >= &pnpm12.into(),
            major => major > 12,
        })
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

    /// Whether convergence must reconcile a changed, newly specified, or removed runner version.
    #[must_use]
    pub fn changes_pnpm_version(&self, other: &Self) -> bool {
        self.pnpm_version != other.pnpm_version
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
        let mut parameters: BTreeMap<_, _> = [
            "peersSuffixMaxLength",
            "addMissingPeerTypes",
            "linkWorkspacePackages",
            "preferWorkspacePackages",
        ]
        .into_iter()
        .filter_map(|field| value.get(field).map(|v| (field.into(), v.clone())))
        .collect();
        if decode::<bool>(value, "dedupeInjectedDeps")? == Some(false) {
            parameters.insert("dedupeInjectedDeps".into(), Value::Bool(false));
        }
        let lockfile = &value["lockfile"];
        if decode::<bool>(lockfile, "includeResolutionSettings")? == Some(true) {
            parameters.insert(INCLUDE_RESOLUTION_SETTINGS.into(), Value::Bool(true));
        }
        match decode::<String>(lockfile, "integrityTarget")?.as_deref() {
            None | Some("resource") => {}
            Some("tar") => {
                parameters.insert(INTEGRITY_TARGET.into(), Value::String("tar".into()));
            }
            Some(_) => {
                return Err(LockfileError::InvalidDocument(format!(
                    "pnpm configuration {INTEGRITY_TARGET}: expected resource or tar"
                )));
            }
        }
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
                parameters,
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
