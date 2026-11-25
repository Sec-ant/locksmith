use std::{
    collections::{BTreeMap, BTreeSet},
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
];

pub const DEFAULT_HOOK_FILES: [&str; 2] = [".pnpmfile.mjs", ".pnpmfile.cjs"];

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
    pub pnpm_version: Option<node_semver::Version>,
}
impl Project {
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
        Self::from_manifest(root, recorded_version, read)
    }

    pub fn from_manifest(
        root: Value,
        recorded_version: Option<node_semver::Version>,
        mut read: impl FnMut(&str) -> Result<Option<String>>,
    ) -> Result<Self> {
        let version = pnpm_version(&root).or(recorded_version);
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
    // Read this hook setting only from the configuration files supported by pnpm.
    config.as_object_mut().unwrap().remove("ignorePnpmfile");
    let legacy = version.is_none_or(|v| v.major() < 11);
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
        legacy_settings(&text, &mut config).map_err(|e| format!(".npmrc: {e}"))?;
    }
    let workspace = read("pnpm-workspace.yaml")?;
    if let Some(text) = &workspace {
        let settings: Option<serde_json::Map<String, Value>> =
            serde_saphyr::from_str(text).map_err(|e| format!("pnpm-workspace.yaml: {e}"))?;
        if let Some(mut settings) = settings {
            // pnpm 10.6 added .npmrc options to pnpm-workspace.yaml.
            if version.is_some_and(|v| v < &node_semver::Version::from((10, 6, 0))) {
                settings.remove("ignorePnpmfile");
            }
            if version.as_ref().is_some_and(|v| v.major() == 9) {
                // pnpm 9 takes these resolver settings from .npmrc.
                for key in [
                    "peersSuffixMaxLength",
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
        ("inject-workspace-packages", "injectWorkspacePackages"),
        ("prefer-workspace-packages", "preferWorkspacePackages"),
    ];
    for line in text.lines().map(str::trim) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some((_, field)) = FLAGS.iter().find(|(flag, _)| key.trim() == *flag) {
            config[field] =
                Value::Bool(ini_bool(value).map_err(|_| format!("{key} expects true or false"))?);
        } else {
            let value = value.trim().trim_matches('"');
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
                _ => (),
            }
        }
    }
    Ok(())
}

fn ini_bool(value: &str) -> std::result::Result<bool, std::str::ParseBoolError> {
    let value = value.trim();
    if let Some(quote @ ('"' | '\'')) = value.chars().next()
        && let Some((quoted, rest)) = value[1..].split_once(quote)
    {
        let rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with(['#', ';']) {
            return quoted.parse();
        }
    }
    value
        .split_once(['#', ';'])
        .map_or(value, |(flag, _)| flag.trim_end())
        .parse()
}

const GLOB_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: true,
};

pub struct Members {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
    descend: Vec<Pattern>,
}
impl Members {
    pub fn new(project: &Project) -> Result<Self> {
        let mut this = Self {
            include: vec![],
            exclude: vec![],
            descend: vec![],
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
                            if parts.pop().is_none() {
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
                    if !value.starts_with('!') {
                        for (index, part) in parts.iter().enumerate() {
                            this.descend.push(Pattern::new(&parts[..=index].join("/"))?);
                            if *part == "**" {
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok(this)
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

    /// Discover manifests from disk, including projects absent from Git or the lockfile.
    pub fn directories_on_disk(&self, root: &Path) -> Result<Vec<String>> {
        if self.include.is_empty() {
            return Ok(Vec::new());
        }
        let mut directories = Vec::new();
        let walk = walkdir::WalkDir::new(root)
            .follow_links(true)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 || !entry.file_type().is_dir() {
                    return true;
                }
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .expect("entry under walk root");
                !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "node_modules" | "bower_components")
                ) && self.descend.iter().any(|p| {
                    p.matches_path_with(
                        relative,
                        MatchOptions {
                            require_literal_leading_dot: false,
                            ..GLOB_OPTIONS
                        },
                    )
                })
            });
        for entry in walk {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if e.loop_ancestor().is_some() => continue,
                Err(e)
                    if e.io_error()
                        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
                        && e.path().is_some_and(Path::is_symlink) =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if entry.depth() < 2
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| MANIFEST_NAMES.contains(&name))
            {
                continue;
            }
            let path = entry
                .path()
                .strip_prefix(root)?
                .parent()
                .expect("non-root manifest");
            if let Some(dir) = path.to_str() {
                let id = dir.replace(std::path::MAIN_SEPARATOR, "/");
                if self.contains(&id) {
                    directories.push(id);
                }
            }
        }
        directories.sort_unstable();
        directories.dedup();
        Ok(directories)
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
        let matches = |p: &Pattern| p.matches_with(&path, GLOB_OPTIONS);
        self.include.iter().any(matches) && !self.exclude.iter().any(matches)
    }
}
