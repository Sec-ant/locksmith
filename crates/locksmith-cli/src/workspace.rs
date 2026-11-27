mod discovery;
mod npmrc;
mod patterns;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Component, Path},
};

use serde_json::{Value, json};

use crate::{Result, git::Blobs};

pub(crate) use discovery::{Members, manifests};

pub const MANIFEST_NAMES: [&str; 3] = ["package.json", "package.json5", "package.yaml"];

pub const CONFIG_FILES: &[&str] = &[
    "package.json",
    "package.json5",
    "package.yaml",
    "pnpm-workspace.yaml",
    ".npmrc",
];

pub const DEFAULT_HOOK_FILES: [&str; 2] = [".pnpmfile.mjs", ".pnpmfile.cjs"];

pub fn check_conflicts(
    conflicts: &BTreeSet<Vec<u8>>,
    relevant: impl Fn(&str) -> bool,
) -> Result<()> {
    let paths: Vec<_> = conflicts
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter(|p| relevant(p))
        .collect();
    if !paths.is_empty() {
        return Err(format!(
            "resolve dependency configuration first:\n  {}",
            paths.join("\n  ")
        )
        .into());
    }
    Ok(())
}

pub fn read_manifest(
    dir: &str,
    read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<Option<Value>> {
    Ok(read_named_manifest(dir, read)?.map(|(_, manifest)| manifest))
}

pub(super) fn read_regular_file(path: &Path) -> io::Result<Option<String>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // Opening a FIFO with no writer must not block the manifest reader.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

pub(super) fn read_named_manifest(
    dir: &str,
    mut read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<Option<(&'static str, Value)>> {
    for name in MANIFEST_NAMES {
        let path = if dir == "." {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        };
        let Some(text) = read(&path)? else { continue };
        return Ok(Some((name, parse_manifest(&path, &text)?)));
    }
    Ok(None)
}

pub(crate) fn parse_manifest(path: &str, text: &str) -> Result<Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let value: Value = if path.ends_with(".json5") {
        json5::from_str(text).map_err(|e| format!("{path}: {e}"))?
    } else if path.ends_with(".yaml") {
        serde_saphyr::from_str(text).map_err(|e| format!("{path}: {e}"))?
    } else {
        serde_json::from_str(text).map_err(|e| format!("{path}: {e}"))?
    };
    if !value.is_object() {
        return Err(format!("{path}: expected an object").into());
    }
    Ok(value)
}

pub struct Project {
    pub root: Value,
    pub config: Value,
    pub workspace: bool,
    pub pnpm_version: Option<node_semver::Version>,
}

pub(super) fn apply_manager_policy(config: &mut locksmith::WorkspaceConfig) {
    if let Ok(policy) =
        std::env::var("pnpm_config_pm_on_fail").or_else(|_| std::env::var("PNPM_CONFIG_PM_ON_FAIL"))
    {
        config.pm_on_fail = Some(policy);
    }
}

pub(super) fn bootstrap_configuration(
    mut config: locksmith::WorkspaceConfig,
    raw: &Value,
    lock: Option<locksmith::MergeInput<'_>>,
    read_rc: impl FnOnce() -> Result<Option<String>>,
) -> Result<locksmith::WorkspaceConfig> {
    apply_manager_policy(&mut config);
    config
        .config_registries
        .extend(crate::registry::bootstrap_registries(raw, lock, read_rc)?);
    Ok(config)
}

impl Project {
    pub fn configuration(&self) -> Result<locksmith::WorkspaceConfig> {
        Ok(match &self.pnpm_version {
            Some(v) => locksmith::WorkspaceConfig::from_pnpm_version(&self.config, v)?,
            None => locksmith::WorkspaceConfig::from_pnpm(&self.config)?,
        })
    }

    pub fn bootstrap_configuration(
        &self,
        lock: Option<locksmith::MergeInput<'_>>,
        read_rc: impl FnOnce() -> Result<Option<String>>,
    ) -> Result<locksmith::WorkspaceConfig> {
        bootstrap_configuration(self.configuration()?, &self.config, lock, read_rc)
    }

    pub fn read(
        blobs: &mut Blobs,
        tree: &str,
        prefix: &str,
        recorded_version: Option<node_semver::Version>,
    ) -> Result<Self> {
        Self::from_files(recorded_version, |path| {
            blobs.read(tree, &format!("{prefix}{path}"))
        })
    }

    pub fn from_files(
        recorded_version: Option<node_semver::Version>,
        mut read: impl FnMut(&str) -> Result<Option<String>>,
    ) -> Result<Self> {
        let root = read_manifest(".", &mut read)?.ok_or("missing root package manifest")?;
        let recorded_version = if recorded_version.is_none()
            && pnpm_version(&root).is_none_or(|v| {
                v.major() < 11 && root["devEngines"].get("packageManager").is_some()
            }) {
            read("pnpm-lock.yaml")?
                .as_deref()
                .and_then(locksmith::RepairInput::recorded_pnpm_version)
        } else {
            recorded_version
        };
        Self::from_manifest(root, recorded_version, read)
    }

    pub fn from_manifest(
        root: Value,
        recorded_version: Option<node_semver::Version>,
        mut read: impl FnMut(&str) -> Result<Option<String>>,
    ) -> Result<Self> {
        let version = locksmith::WorkspaceConfig::project_pnpm_version(&root, recorded_version);
        let (mut config, workspace) = settings(&root, version.as_ref(), &mut read)?;
        // The core uses hook identity to select already-locked evidence. A
        // newly composed hook with no matching source must remain unresolved.
        let mut hooks = serde_json::Map::new();
        let candidates = if config["ignorePnpmfile"] == true {
            Vec::new()
        } else {
            hook_candidates(&config, version.as_ref().map(|v| v.major()))?
        };
        for name in candidates {
            if let Some(contents) = read(&name)? {
                hooks.insert(name, Value::String(contents));
                if config.get("pnpmfile").is_none() {
                    break;
                }
            }
        }
        if !hooks.is_empty() {
            config["pnpmfileChecksum"] = Value::Object(hooks);
        }
        Ok(Self {
            root,
            config,
            workspace,
            pnpm_version: version,
        })
    }
}

pub fn pnpm_version(root: &Value) -> Option<node_semver::Version> {
    locksmith::WorkspaceConfig::project_pnpm_version(root, None)
}

/// Read the package-manager settings shared by workspace operations and inspection.
pub fn settings(
    root: &Value,
    version: Option<&node_semver::Version>,
    mut read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<(Value, bool)> {
    let legacy = version.is_none_or(|v| v.major() < 11);
    let pnpm9 = version.is_some_and(|v| v.major() == 9);
    let mut config = match root.get("pnpm").filter(|_| legacy) {
        Some(v) if v.is_object() => v.clone(),
        Some(_) => {
            return Err("package.json: expected pnpm to be an object".into());
        }
        None => json!({}),
    };
    let manifest_settings = config.as_object_mut().expect("manifest settings");
    // Membership is declared in pnpm-workspace.yaml, including the omitted-key default.
    manifest_settings.remove("packages");
    // Read this hook setting only from the configuration files supported by pnpm.
    manifest_settings.remove("ignorePnpmfile");
    if version.is_some() {
        // The peer suffix limit is a config file setting.
        manifest_settings.remove("peersSuffixMaxLength");
    }
    if legacy && let Some(resolutions) = root.get("resolutions") {
        let mut overrides = resolutions
            .as_object()
            .ok_or("package.json: expected resolutions to be an object")?
            .clone();
        if let Some(value) = config.get("overrides") {
            overrides.extend(
                value
                    .as_object()
                    .ok_or("package.json: expected pnpm.overrides to be an object")?
                    .clone(),
            );
        }
        config["overrides"] = overrides.into();
    }
    if legacy && let Some(text) = read(".npmrc")? {
        npmrc::legacy_settings(&text, &mut config, version).map_err(|e| format!(".npmrc: {e}"))?;
    }
    let workspace = read("pnpm-workspace.yaml")?;
    if let Some(text) = &workspace {
        let settings: Option<serde_json::Map<String, Value>> =
            serde_saphyr::from_str(text).map_err(|e| format!("pnpm-workspace.yaml: {e}"))?;
        if let Some(mut settings) = settings {
            if version.is_some_and(|v| v < &node_semver::Version::from((10, 5, 0)))
                && !settings.is_empty()
                && !settings.contains_key("packages")
            {
                return Err("pnpm-workspace.yaml: this pnpm version requires packages in a nonempty workspace configuration".into());
            }
            // pnpm 10.6 added .npmrc options to pnpm-workspace.yaml.
            if version.is_some_and(|v| v < &node_semver::Version::from((10, 6, 0))) {
                settings.remove("ignorePnpmfile");
                settings.remove("peersSuffixMaxLength");
            }
            if pnpm9 {
                // pnpm 9 takes these resolver settings from .npmrc.
                for key in [
                    "injectWorkspacePackages",
                    "autoInstallPeers",
                    "addMissingPeerTypes",
                    "ignoredOptionalDependencies",
                ] {
                    settings.remove(key);
                }
            }
            config.as_object_mut().unwrap().extend(settings);
        }
    }
    Ok((config, workspace.is_some()))
}

pub fn patches(
    config: &locksmith::WorkspaceConfig,
    prefix: &str,
    mut read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<BTreeMap<String, String>> {
    let mut patches = BTreeMap::new();
    for path in config.patches.iter().flat_map(|m| m.values()) {
        if !patches.contains_key(path) {
            let file = format!("{prefix}{}", relative(path)?);
            let contents =
                read(&file)?.ok_or_else(|| format!("missing configured patch {file}"))?;
            patches.insert(path.clone(), contents);
        }
    }
    Ok(patches)
}

pub fn relative(path: &str) -> Result<String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(s) => parts.push(s.to_str().unwrap()),
            Component::CurDir => (),
            _ => {
                return Err(format!(
                    "expected a workspace-relative configuration path, got {path:?}"
                )
                .into());
            }
        }
    }
    if parts.is_empty() {
        return Err("empty workspace file path".into());
    }
    Ok(parts.join("/"))
}

fn hook_candidates(config: &Value, major: Option<u64>) -> Result<Vec<String>> {
    match config.get("pnpmfile") {
        Some(Value::String(s)) => Ok(vec![relative(s)?]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|s| relative(s.as_str().ok_or("pnpmfile expects paths")?))
            .collect(),
        Some(_) => Err("pnpmfile expects a path or a list of paths".into()),
        None if major.is_some_and(|v| v < 11) => Ok(vec![".pnpmfile.cjs".into()]),
        None => Ok(DEFAULT_HOOK_FILES.into_iter().map(str::to_owned).collect()),
    }
}

pub fn hook_paths(config: &Value) -> Result<Vec<String>> {
    if config["ignorePnpmfile"] == true {
        return Ok(Vec::new());
    }
    if config.get("pnpmfile").is_some() {
        return hook_candidates(config, None);
    }
    Ok(config["pnpmfileChecksum"]
        .as_object()
        .into_iter()
        .flat_map(|files| files.keys().cloned())
        .collect())
}

pub fn file_inputs(config: &Value) -> Result<Vec<String>> {
    let mut paths = hook_paths(config)?;
    if let Some(patches) = config.get("patchedDependencies") {
        for path in patches
            .as_object()
            .ok_or("patchedDependencies expects a mapping")?
            .values()
        {
            paths.push(relative(
                path.as_str().ok_or("patch path must be a string")?,
            )?);
        }
    }
    Ok(paths)
}
