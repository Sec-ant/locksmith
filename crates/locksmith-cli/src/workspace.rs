use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Component, Path},
};

use glob::{MatchOptions, Pattern};
use serde_json::{Value, json};

use super::{
    Result,
    git::{self, Blobs},
};

pub const MANIFEST_NAMES: [&str; 3] = ["package.json", "package.json5", "package.yaml"];

pub const CONFIG_FILES: &[&str] = &[
    "package.json",
    "package.json5",
    "package.yaml",
    "pnpm-workspace.yaml",
    ".npmrc",
    ".pnpmfile.cjs",
    ".pnpmfile.mjs",
];

pub fn check_conflicts(
    conflicts: &BTreeSet<String>,
    relevant: impl Fn(&str) -> bool,
) -> Result<()> {
    let paths: Vec<_> = conflicts
        .iter()
        .filter(|p| relevant(p))
        .map(String::as_str)
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

pub fn working_file(path: &str) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}").into()),
    }
}

pub fn read_manifest(
    dir: &str,
    mut read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<Option<Value>> {
    for name in MANIFEST_NAMES {
        let path = if dir == "." {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        };
        let Some(text) = read(&path)? else { continue };
        return parse_manifest(&path, &text).map(Some);
    }
    Ok(None)
}

fn parse_manifest(path: &str, text: &str) -> Result<Value> {
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
}
impl Project {
    pub fn read(blobs: &mut Blobs, tree: &str, prefix: &str) -> Result<Self> {
        Self::from_files(|path| blobs.read(tree, &format!("{prefix}{path}")))
    }

    pub fn from_files(mut read: impl FnMut(&str) -> Result<Option<String>>) -> Result<Self> {
        let root = read_manifest(".", &mut read)?.ok_or("missing root package manifest")?;
        let version = pnpm_version(&root);
        Self::from_files_with_version(root, version.as_ref(), read)
    }

    pub fn from_files_with_version(
        root: Value,
        version: Option<&node_semver::Version>,
        mut read: impl FnMut(&str) -> Result<Option<String>>,
    ) -> Result<Self> {
        let (mut config, workspace) = settings(&root, version, &mut read)?;
        // The core uses hook identity to select already-locked evidence. A
        // newly composed hook with no matching source must remain unresolved.
        let mut hooks = serde_json::Map::new();
        for name in hook_candidates(&config, version.as_ref().map(|v| v.major))? {
            if let Some(contents) = read(&name)? {
                if config["ignorePnpmfile"] == true || contents.trim().is_empty() {
                    continue;
                }
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
        })
    }
}

pub fn pnpm_version(root: &Value) -> Option<node_semver::Version> {
    root["packageManager"]
        .as_str()
        .and_then(|v| v.strip_prefix("pnpm@"))
        .and_then(|v| v.parse::<node_semver::Version>().ok())
}

/// Read the package-manager settings shared by workspace operations and inspection.
pub fn settings(
    root: &Value,
    version: Option<&node_semver::Version>,
    mut read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<(Value, bool)> {
    let mut config = match root.get("pnpm") {
        Some(v) if v.is_object() => v.clone(),
        Some(_) => {
            return Err("package.json: expected pnpm to be an object".into());
        }
        None => json!({}),
    };
    if version.is_none_or(|v| v.major < 11)
        && let Some(resolutions) = root.get("resolutions")
    {
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
    if version.as_ref().is_none_or(|v| v.major < 11)
        && let Some(text) = read(".npmrc")?
    {
        legacy_settings(&text, &mut config).map_err(|e| format!(".npmrc: {e}"))?;
    }
    let workspace = read("pnpm-workspace.yaml")?;
    if let Some(text) = &workspace {
        let settings: Option<serde_json::Map<String, Value>> =
            serde_saphyr::from_str(text).map_err(|e| format!("pnpm-workspace.yaml: {e}"))?;
        if let Some(mut settings) = settings {
            if version.as_ref().is_some_and(|v| v.major == 9) {
                // pnpm 9 takes these resolver settings from .npmrc.
                for key in [
                    "peersSuffixMaxLength",
                    "injectWorkspacePackages",
                    "autoInstallPeers",
                    "addMissingPeerTypes",
                ] {
                    settings.remove(key);
                }
            }
            if version.is_none_or(|v| *v >= node_semver::Version::new(12, 15, 0)) {
                include_settings(&mut settings, &mut read)?;
            }
            config.as_object_mut().unwrap().extend(settings);
        }
    }
    Ok((config, workspace.is_some()))
}

/// Settings an included file may hold only as paths relative to that file.
const PATH_SETTINGS: &[&str] = &[
    "globalPnpmfile",
    "lockfileDir",
    "modulesDir",
    "packages",
    "patchedDependencies",
    "patchesDir",
    "pnpmfile",
    "scriptShell",
    "storeDir",
    "virtualStoreDir",
];

/// Each catalog has one owner. Other settings are owned by name.
fn owned_units(key: &str, value: &Value) -> Vec<String> {
    match key {
        "catalog" => vec!["catalogs.default".into()],
        "catalogs" => value
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, _)| format!("catalogs.{name}"))
            .collect(),
        _ => vec![key.into()],
    }
}

fn holds_local_path(value: &Value) -> bool {
    match value {
        Value::String(s) => [
            "file:",
            "link:",
            "./",
            "../",
            "workspace:./",
            "workspace:../",
        ]
        .iter()
        .any(|prefix| s.starts_with(prefix)),
        Value::Object(map) => map.values().any(holds_local_path),
        Value::Array(items) => items.iter().any(holds_local_path),
        _ => false,
    }
}

/// Merge the settings that pnpm-workspace.yaml delegates to other files with
/// `include` (pnpm 12.15). pnpm rejects a setting that two sources define, or
/// that a file defines without being delegated it.
fn include_settings(
    settings: &mut serde_json::Map<String, Value>,
    mut read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<()> {
    let Some(include) = settings.remove("include") else {
        return Ok(());
    };
    let include = include
        .as_object()
        .ok_or("pnpm-workspace.yaml: expected include to be a mapping")?;
    let mut owners = BTreeMap::<String, String>::new();
    let reserved = |owners: &BTreeMap<String, String>, unit: &str| {
        owners.get(unit).cloned().or_else(|| {
            unit.starts_with("catalogs.")
                .then(|| owners.get("catalogs").cloned())
                .flatten()
        })
    };
    let mut sources = Vec::new();
    for (source, delegated) in include {
        let invalid = |reason: &str| -> Box<dyn std::error::Error> {
            format!("pnpm-workspace.yaml: include {source:?} {reason}").into()
        };
        if !source.starts_with("./") && !source.starts_with("../") {
            return Err(invalid(
                "names a config dependency; generate cannot read config dependency settings",
            ));
        }
        let mut parts = Vec::new();
        for component in Path::new(source).components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    parts
                        .pop()
                        .ok_or_else(|| invalid("is outside the workspace root"))?;
                }
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                _ => return Err(invalid("must be a relative path")),
            }
        }
        let path = parts.join("/");
        if !(path.ends_with(".yaml") || path.ends_with(".yml"))
            || path.contains(['*', '?', '[', '{'])
        {
            return Err(invalid("must name a .yaml or .yml file"));
        }
        let permitted = match delegated {
            Value::String(all) if all == "*" => None,
            Value::Array(items) => Some(
                items
                    .iter()
                    .map(|item| match item.as_str() {
                        Some("catalog") => Ok("catalogs.default".to_owned()),
                        Some(setting) => Ok(setting.to_owned()),
                        None => Err(invalid("expects a list of setting names or '*'")),
                    })
                    .collect::<Result<BTreeSet<_>>>()?,
            ),
            _ => return Err(invalid("expects a list of setting names or '*'")),
        };
        for unit in permitted.iter().flatten() {
            if let Some(other) = owners.insert(unit.clone(), source.clone()) {
                return Err(format!("{unit} is delegated to both {other} and {source}").into());
            }
        }
        sources.push((source, path, permitted));
    }
    for (key, value) in settings.iter() {
        for unit in owned_units(key, value) {
            if let Some(owner) = reserved(&owners, &unit) {
                return Err(format!(
                    "pnpm-workspace.yaml defines {unit}, which include delegates to {owner}"
                )
                .into());
            }
        }
    }
    for (source, path, permitted) in sources {
        let text = read(&path)?.ok_or_else(|| format!("{source}: included file is missing"))?;
        let file: Option<serde_json::Map<String, Value>> =
            serde_saphyr::from_str(&text).map_err(|e| format!("{source}: {e}"))?;
        let nested = path.contains('/');
        for (key, value) in file.into_iter().flatten() {
            if key == "include" {
                return Err(format!("{source}: an included file cannot define include").into());
            }
            for unit in owned_units(&key, &value) {
                match &permitted {
                    // A list reserves its settings for this file and no other.
                    Some(_) => {
                        if reserved(&owners, &unit).as_ref() != Some(source) {
                            return Err(
                                format!("{source}: {unit} is not delegated to this file").into()
                            );
                        }
                    }
                    None => {
                        let owner = reserved(&owners, &unit).or_else(|| {
                            settings
                                .iter()
                                .any(|(k, v)| owned_units(k, v).contains(&unit))
                                .then(|| "pnpm-workspace.yaml".to_owned())
                        });
                        if let Some(owner) = owner {
                            return Err(
                                format!("{unit} is defined by both {owner} and {source}").into()
                            );
                        }
                        owners.insert(unit, source.clone());
                    }
                }
            }
            // pnpm resolves relative paths against the included file.
            if nested
                && (PATH_SETTINGS.contains(&key.as_str())
                    || ["catalog", "catalogs", "overrides"].contains(&key.as_str())
                        && holds_local_path(&value))
            {
                return Err(format!(
                    "{source}: generate cannot rebase the relative paths in {key} \
                     from a file outside the workspace root directory"
                )
                .into());
            }
            match (key.as_str(), value) {
                ("catalogs", Value::Object(catalogs)) => {
                    let target = settings
                        .entry("catalogs")
                        .or_insert_with(|| json!({}))
                        .as_object_mut()
                        .ok_or("pnpm-workspace.yaml: expected catalogs to be a mapping")?;
                    target.extend(catalogs);
                }
                (_, value) => {
                    settings.insert(key, value);
                }
            }
        }
    }
    Ok(())
}

pub fn manifests(
    project: &Project,
    blobs: &mut Blobs,
    tree: &str,
    prefix: &str,
) -> Result<BTreeMap<String, Value>> {
    let mut manifests = BTreeMap::from([(".".into(), project.root.clone())]);
    if project.workspace {
        let members = Members::new(project)?;
        let tree_paths = git::workspace_paths(tree, prefix)?;
        let paths = members.manifest_paths(&tree_paths, prefix);
        git::prefetch_manifests(tree, prefix, &paths)?;
        for (id, path) in paths {
            let text = blobs
                .read(tree, &format!("{prefix}{path}"))?
                .ok_or_else(|| format!("missing manifest in {tree}:{prefix}{path}"))?;
            manifests.insert(id.to_owned(), parse_manifest(path, &text)?);
        }
    }
    Ok(manifests)
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
        None => Ok(vec![".pnpmfile.mjs".into(), ".pnpmfile.cjs".into()]),
    }
}

pub fn hook_paths(config: &Value) -> Result<Vec<String>> {
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

// pnpm 9 and 10 also read these resolver and peer settings from project .npmrc.
fn legacy_settings(text: &str, config: &mut Value) -> Result<()> {
    const FLAGS: &[(&str, &str)] = &[
        ("auto-install-peers", "autoInstallPeers"),
        ("ignore-pnpmfile", "ignorePnpmfile"),
        ("dedupe-peers", "dedupePeers"),
        ("dedupe-peer-dependents", "dedupePeerDependents"),
        ("exclude-links-from-lockfile", "excludeLinksFromLockfile"),
        (
            "project-scoped-optional-peers",
            "projectScopedOptionalPeers",
        ),
        (
            "resolve-peers-from-workspace-root",
            "resolvePeersFromWorkspaceRoot",
        ),
        ("strict-peer-dependencies", "strictPeerDependencies"),
        ("ignore-compatibility-db", "ignoreCompatibilityDb"),
        ("inject-workspace-packages", "injectWorkspacePackages"),
        ("prefer-workspace-packages", "preferWorkspacePackages"),
    ];
    for line in text.lines().map(str::trim) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if let Some((_, field)) = FLAGS.iter().find(|(flag, _)| key.trim() == *flag) {
            config[field] = Value::Bool(
                value
                    .parse()
                    .map_err(|_| format!("{key} expects true or false"))?,
            );
        } else {
            match key.trim() {
                "link-workspace-packages" => {
                    config["linkWorkspacePackages"] = match value {
                        "deep" => json!("deep"),
                        _ => json!(value.parse::<bool>()?),
                    }
                }
                "peers-suffix-max-length" => {
                    config["peersSuffixMaxLength"] = json!(value.parse::<usize>()?)
                }
                "pnpmfile" => config["pnpmfile"] = json!(value),
                "global-pnpmfile" => config["globalPnpmfile"] = json!(value),
                "store-dir" => config["storeDir"] = json!(value),
                "modules-dir" => config["modulesDir"] = json!(value),
                "virtual-store-dir" => config["virtualStoreDir"] = json!(value),
                _ => (),
            }
        }
    }
    Ok(())
}

pub struct Members {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}
impl Members {
    pub fn new(project: &Project) -> Result<Self> {
        Self::from_project(project, false)
    }

    pub fn for_generation(project: &Project) -> Result<Self> {
        Self::from_project(project, true)
    }

    fn from_project(project: &Project, external: bool) -> Result<Self> {
        let mut this = Self {
            include: vec![],
            exclude: vec![],
        };
        if !project.workspace {
            return Ok(this);
        }
        let patterns: Vec<String> = match project.config.get("packages") {
            None => vec!["**".into()],
            Some(v) => serde_json::from_value(v.clone())?,
        };
        for value in patterns {
            let (patterns, pattern) = match value.strip_prefix('!') {
                Some(p) => (&mut this.exclude, p),
                None => (&mut this.include, value.as_str()),
            };
            if pattern.contains(['(', ')', '\\']) || pattern.starts_with(['!', '/']) {
                return Err(format!("unsupported workspace glob {value:?}").into());
            }
            // Expand before normalizing: alternatives can have different depths.
            let expanded = brace_expansion::expand_with_max(pattern, 1025);
            if expanded.len() > 1024 {
                return Err(format!("too many alternatives in workspace glob {value:?}").into());
            }
            for alternative in expanded {
                let mut parts = Vec::new();
                for part in alternative.split('/') {
                    match part {
                        "" | "." => (),
                        ".." => {
                            if parts.last().is_some_and(|p| *p != "..") {
                                parts.pop();
                            } else if external {
                                parts.push(part);
                            } else {
                                return Err(
                                    format!("workspace glob escapes the root: {value:?}").into()
                                );
                            }
                        }
                        _ => parts.push(part),
                    }
                }
                if !parts.is_empty() {
                    patterns.push(Pattern::new(&format!("{}/package.json", parts.join("/")))?);
                }
            }
        }
        Ok(this)
    }

    /// Expand the declared globs on disk. Literal prefixes and bounded patterns
    /// avoid walking unrelated trees in a workspace that does not use **.
    pub fn filesystem_manifests(&self, root: &Value) -> Result<BTreeMap<String, Value>> {
        let mut manifests = BTreeMap::from([(".".into(), root.clone())]);
        let mut directories = BTreeSet::new();
        let options = MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: true,
        };
        for pattern in &self.include {
            let directory = pattern
                .as_str()
                .strip_suffix("/package.json")
                .expect("manifest glob");
            // Iterate one component at a time so ** cannot enter node_modules or
            // follow symlink cycles before the membership filter is applied.
            let parts: Vec<_> = directory.split('/').collect();
            let mut pending = vec![(std::path::PathBuf::new(), 0)];
            let mut seen = BTreeSet::new();
            while let Some((dir, depth)) = pending.pop() {
                if !seen.insert((dir.clone(), depth)) {
                    continue;
                }
                let Some(part) = parts.get(depth) else {
                    if let Some(s) = dir.to_str().filter(|s| !s.is_empty()) {
                        let id = s.replace(std::path::MAIN_SEPARATOR, "/");
                        if self.contains(&id) {
                            directories.insert(id);
                        }
                    }
                    continue;
                };
                let recursive = *part == "**";
                if *part == ".." {
                    pending.push((dir.join(".."), depth + 1));
                    continue;
                }
                if recursive {
                    pending.push((dir.clone(), depth + 1));
                }
                let parent = if dir.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    &dir
                };
                let component = Pattern::new(part)?;
                for entry in fs::read_dir(parent)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if matches!(name, "node_modules" | "bower_components" | ".git")
                        || !component.matches_with(name, options)
                    {
                        continue;
                    }
                    pending.push((dir.join(name), if recursive { depth } else { depth + 1 }));
                }
            }
        }
        for dir in directories {
            if let Some(manifest) = read_manifest(&dir, working_file)? {
                manifests.insert(dir, manifest);
            }
        }
        Ok(manifests)
    }

    pub fn working_directories(&self, prefix: &str) -> Result<BTreeSet<String>> {
        let mut args = vec!["ls-files", "-zc", "--others", "--exclude-standard"];
        let exclude;
        if !self.include.iter().any(|p| p.as_str().contains("**"))
            && let Some(depth) = self
                .include
                .iter()
                .map(|p| p.as_str().matches('/').count())
                .max()
        {
            // Patterns include the manifest filename. Git's exclude is rooted
            // at the repository, so count the workspace prefix as well.
            let first_unused_depth = prefix.matches('/').count() + depth + 1;
            exclude = format!("--exclude={}/", "/*".repeat(first_unused_depth));
            args.push(&exclude);
        }
        args.extend([
            "--",
            ":(glob)**/package.json",
            ":(glob)**/package.json5",
            ":(glob)**/package.yaml",
        ]);
        let paths = git::output(&args)?;
        Ok(self
            .manifest_paths(&paths, "")
            .into_keys()
            .map(str::to_owned)
            .collect())
    }

    fn manifest_paths<'a>(&self, paths: &'a str, prefix: &str) -> BTreeMap<&'a str, &'a str> {
        let mut manifests = BTreeMap::new();
        for path in paths
            .split_terminator('\0')
            .filter_map(|p| p.strip_prefix(prefix))
        {
            let Some((dir, name)) = path.rsplit_once('/') else {
                continue;
            };
            if MANIFEST_NAMES.contains(&name) && self.contains(dir) {
                // package.json < package.json5 < package.yaml in pnpm's search order.
                let selected = manifests.entry(dir).or_insert(path);
                if path < *selected {
                    *selected = path;
                }
            }
        }
        manifests
    }

    pub fn contains(&self, dir: &str) -> bool {
        if dir
            .split('/')
            .any(|p| matches!(p, "node_modules" | "bower_components"))
        {
            return false;
        }
        let path = format!("{dir}/package.json");
        let matches = |p: &Pattern| {
            p.matches_with(
                &path,
                MatchOptions {
                    case_sensitive: true,
                    require_literal_separator: true,
                    require_literal_leading_dot: true,
                },
            )
        };
        self.include.iter().any(matches) && !self.exclude.iter().any(matches)
    }
}
