//! Check bootstrap dependencies and settings recorded in the lockfile.
use crate::{LockfileError, Value, model::error::missing};

use super::{INCLUDE_RESOLUTION_SETTINGS, INTEGRITY_TARGET, LockSettings, WorkspaceConfig};

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

    pub(crate) fn equivalent_recorded_settings(
        &self,
        left: &Value,
        right: &Value,
    ) -> Result<bool, LockfileError> {
        if LockSettings::from_document(left, self.pnpm_version.as_ref())?
            != LockSettings::from_document(right, self.pnpm_version.as_ref())?
        {
            return Ok(false);
        }
        let defaults = [
            ("peersSuffixMaxLength", Value::from(1000)),
            ("injectWorkspacePackages", Value::Bool(false)),
            ("addMissingPeerTypes", Value::Bool(false)),
            ("autoDedupe", Value::Bool(false)),
            ("dedupeInjectedDeps", Value::Bool(true)),
            ("dedupePeerDependents", Value::Bool(true)),
            ("linkWorkspacePackages", Value::Bool(false)),
            ("integrityTarget", Value::from("resource")),
        ];
        let [left, right] = [left, right].map(|document| {
            let mut settings = document["settings"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for name in [
                "autoInstallPeers",
                "dedupePeers",
                "projectScopedOptionalPeers",
                "excludeLinksFromLockfile",
            ] {
                settings.remove(name);
            }
            for (name, default) in &defaults {
                if settings.get(*name) == Some(default) {
                    settings.remove(*name);
                }
            }
            settings
        });
        Ok(left == right)
    }

    pub(crate) fn check_recorded_peer_suffix(&self, doc: &Value) -> Result<(), LockfileError> {
        let key = "peersSuffixMaxLength";
        let default_suffix = Value::from(1000);
        let expected = self
            .resolution
            .parameters
            .get(key)
            .unwrap_or(&default_suffix);
        Self::check_recorded_setting(&doc["settings"], key, expected, &default_suffix)
    }

    pub(crate) fn check_recorded_settings(&self, doc: &Value) -> Result<(), LockfileError> {
        self.check_recorded_peer_suffix(doc)?;
        let settings = &doc["settings"];
        let major = self.pnpm_version.as_ref().map(|v| v.major());
        let disabled = Value::Bool(false);
        for (key, expected, supported) in [
            (
                "injectWorkspacePackages",
                &Value::Bool(self.resolution.inject_workspace_packages),
                major.is_none_or(|m| m >= 10),
            ),
            (
                "addMissingPeerTypes",
                self.resolution
                    .parameters
                    .get("addMissingPeerTypes")
                    .unwrap_or(&disabled),
                major.is_none_or(|m| m >= 12),
            ),
        ] {
            if supported {
                Self::check_recorded_setting(settings, key, expected, &disabled)?;
            }
        }
        Ok(())
    }

    fn check_recorded_setting(
        settings: &Value,
        key: &str,
        expected: &Value,
        default: &Value,
    ) -> Result<(), LockfileError> {
        if settings.get(key).unwrap_or(default) != expected {
            return Err(missing(
                &["settings", key],
                "lockfile setting differs from the workspace; regenerate the lockfile with pnpm",
            ));
        }
        Ok(())
    }

    pub(crate) fn frozen_settings_mismatch(&self, document: &Value) -> Option<&'static str> {
        let settings = &document["settings"];
        let parameters = &self.resolution.parameters;
        let version = self.pnpm_version.as_ref();
        if parameters.get(INCLUDE_RESOLUTION_SETTINGS) == Some(&Value::Bool(true))
            && version.is_none_or(|version| version >= &node_semver::Version::from((12, 10, 0)))
        {
            let enabled = Value::Bool(true);
            let disabled = Value::Bool(false);
            let expected = [
                ("autoDedupe", &Value::Bool(self.auto_dedupe)),
                (
                    "dedupeInjectedDeps",
                    parameters.get("dedupeInjectedDeps").unwrap_or(&enabled),
                ),
                (
                    "dedupePeerDependents",
                    &Value::Bool(self.resolution.dedupe_peer_dependents),
                ),
                (
                    "linkWorkspacePackages",
                    parameters.get("linkWorkspacePackages").unwrap_or(&disabled),
                ),
            ];
            for (name, value) in expected {
                if settings.get(name) != Some(value) {
                    return Some(name);
                }
            }
        }
        let recorded_integrity = settings
            .get("integrityTarget")
            .map_or(Some("resource"), Value::as_str);
        let configured_integrity = parameters
            .get(INTEGRITY_TARGET)
            .map_or(Some("resource"), Value::as_str);
        if version.is_none_or(|version| version >= &node_semver::Version::from((12, 15, 0)))
            && recorded_integrity != configured_integrity
        {
            return Some("integrityTarget");
        }
        None
    }

    /// Whether convergence needs to reconcile the installed bootstrap graph and patch records.
    #[must_use]
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
}
