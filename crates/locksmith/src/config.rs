//! Explicit workspace inputs and the pnpm configuration decoder.
use crate::{LockfileError, Value};
use std::collections::BTreeMap;

/// Which declaration wins when matching override ranges have equal priority.
/// pnpm 9–11 prefer the last matching declaration; pnpm 12 prefers the first.
/// Callers select this explicitly; graph operations do not infer a pnpm version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverrideTieBreak {
    First,
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
    Md5,
    Sha256,
}

/// Which installed packages may supply a newly required optional peer.
/// Adapters choose this explicitly when package-manager versions differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OptionalPeerScope {
    #[default]
    Workspace,
    Project,
}

/// Settings that affect the meaning of importer dependency declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockSettings {
    pub auto_install_peers: bool,
    pub dedupe_peers: bool,
    pub project_scoped_optional_peers: bool,
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
    pub(crate) fn from_document(document: &Value) -> Self {
        Self {
            auto_install_peers: document["settings"]["autoInstallPeers"]
                .as_bool()
                .unwrap_or(true),
            dedupe_peers: document["settings"]["dedupePeers"]
                .as_bool()
                .unwrap_or(false),
            project_scoped_optional_peers: document["settings"]["projectScopedOptionalPeers"]
                .as_bool()
                .unwrap_or(false),
            exclude_links_from_lockfile: document["settings"]["excludeLinksFromLockfile"]
                .as_bool()
                .unwrap_or(false),
        }
    }
}

/// Settings used to decide whether locked records can be reused. Package
/// extensions apply to matching names and versions; the other settings and
/// additional parameters are equality constraints on the source graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionSettings {
    pub lock: LockSettings,
    pub workspace_root_peers: bool,
    pub dedupe_peer_dependents: bool,
    pub optional_peer_scope: OptionalPeerScope,
    pub inject_workspace_packages: bool,
    pub package_extensions: Option<Value>,
    pub ignored_optional_dependencies: Option<Value>,
    pub pnpmfile_checksum: Option<Value>,
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
    pub auto_dedupe: bool,
    pub resolution: ResolutionSettings,
    pub catalogs: BTreeMap<String, BTreeMap<String, String>>,
    /// Selector/replacement pairs in declaration order (ties can depend on it).
    pub overrides: Option<Vec<(String, String)>>,
    pub override_tie_break: OverrideTieBreak,
    /// pnpm release responsible for this resolution, when known.
    pub pnpm_version: Option<node_semver::Version>,
    pub linked_peer_encoding: LinkedPeerEncoding,
    pub patches: Option<BTreeMap<String, String>>,
    pub patch_encoding: PatchEncoding,
    pub extension_checksum: Option<ExtensionChecksum>,
}
impl WorkspaceConfig {
    /// Decode workspace settings using the lockfile conventions of this pnpm release.
    pub fn from_pnpm_version(
        value: &Value,
        v: &node_semver::Version,
    ) -> Result<Self, LockfileError> {
        let mut config = Self::from_pnpm(value)?;
        config.pnpm_version = Some(v.clone());
        config.extension_checksum = Some(if v.major < 10 {
            ExtensionChecksum::Md5
        } else {
            ExtensionChecksum::Sha256
        });
        config.override_tie_break = if v.major >= 12 {
            OverrideTieBreak::First
        } else {
            OverrideTieBreak::Last
        };
        config.patch_encoding = match v.major {
            0..=9 => PatchEncoding::Md5Base32WithPath,
            10 => PatchEncoding::Sha256WithPath,
            _ => PatchEncoding::Sha256,
        };
        config.linked_peer_encoding = if v.major >= 11 {
            LinkedPeerEncoding::Path
        } else {
            LinkedPeerEncoding::Filename
        };
        if v.major < 11
            || (v.major == 11 && v < &node_semver::Version::new(11, 28, 12))
            || (v.major == 12 && v < &node_semver::Version::new(12, 19, 0))
        {
            config.resolution.optional_peer_scope = OptionalPeerScope::Workspace;
        }
        Ok(config)
    }

    pub(crate) fn matches_catalog_ranges(&self) -> bool {
        self.pnpm_version.as_ref().is_some_and(|v| match v.major {
            11 => v >= &node_semver::Version::new(11, 28, 12),
            12 => v >= &node_semver::Version::new(12, 20, 0),
            major => major > 12,
        })
    }

    pub(crate) fn records_link_directory(&self) -> bool {
        self.pnpm_version.as_ref().is_some_and(|v| match v.major {
            11 => v >= &node_semver::Version::new(11, 26, 0),
            12 => v >= &node_semver::Version::new(12, 3, 2),
            major => major > 12,
        })
    }

    /// Whether both configurations name pnpm releases in different major versions.
    pub fn changes_pnpm_major(&self, other: &Self) -> bool {
        self.pnpm_version
            .as_ref()
            .zip(other.pnpm_version.as_ref())
            .is_some_and(|(a, b)| a.major != b.major)
    }

    pub(crate) fn matches_pnpm(&self, target: &Self) -> bool {
        target.pnpm_version.is_none() || self.pnpm_version == target.pnpm_version
    }

    /// Decode one effective pnpm configuration object. Files, configuration
    /// precedence, and workspace-glob expansion belong to the caller.
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
                ignored_optional_dependencies: value.get("ignoredOptionalDependencies").cloned(),
                pnpmfile_checksum: value.get("pnpmfileChecksum").cloned(),
                parameters: [
                    "peersSuffixMaxLength",
                    "linkWorkspacePackages",
                    "preferWorkspacePackages",
                ]
                .into_iter()
                .filter_map(|field| value.get(field).map(|v| (field.into(), v.clone())))
                .collect(),
            },
            catalogs,
            overrides: string_rules(value, "overrides")?,
            override_tie_break: OverrideTieBreak::default(),
            pnpm_version: None,
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
