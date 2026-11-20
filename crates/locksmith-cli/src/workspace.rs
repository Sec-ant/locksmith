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

pub const CONFIG_FILES: &[&str] = &[
    "package.json",
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

pub fn manifest(blobs: &mut Blobs, tree: &str, path: &str) -> Result<Value> {
    let text = blobs
        .read(tree, path)?
        .ok_or_else(|| format!("missing {path} in {tree}"))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
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
        let root: Value =
            serde_json::from_str(&read("package.json")?.ok_or("missing package.json")?)
                .map_err(|e| format!("package.json: {e}"))?;
        if !root.is_object() {
            return Err("package.json: expected an object".into());
        }
        let mut config = match root.get("pnpm") {
            Some(v) if v.is_object() => v.clone(),
            Some(_) => {
                return Err("package.json: expected pnpm to be an object".into());
            }
            None => json!({}),
        };
        let version = root["packageManager"]
            .as_str()
            .and_then(|v| v.strip_prefix("pnpm@"))
            .and_then(|v| v.parse::<node_semver::Version>().ok());
        if version.as_ref().is_none_or(|v| v.major < 11)
            && let Some(text) = read(".npmrc")?
        {
            legacy_settings(&text, &mut config).map_err(|e| format!(".npmrc: {e}"))?;
        }
        let workspace = read("pnpm-workspace.yaml")?;
        if let Some(text) = &workspace {
            let settings: Option<serde_json::Map<String, Value>> =
                serde_saphyr::from_str(text).map_err(|e| format!("pnpm-workspace.yaml: {e}"))?;
            if let Some(settings) = settings {
                config.as_object_mut().unwrap().extend(settings);
            }
        }
        // The core uses hook identity to select already-locked evidence. A
        // newly composed hook with no matching source must remain unresolved.
        let mut hooks = serde_json::Map::new();
        for name in hook_paths(&config)? {
            if let Some(contents) = read(&name)? {
                hooks.insert(name, Value::String(contents));
            }
        }
        if !hooks.is_empty() {
            config["pnpmfileChecksum"] = Value::Object(hooks);
        }
        Ok(Self {
            root,
            config,
            workspace: workspace.is_some(),
        })
    }
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
        for id in members.directories(&super::git::workspace_paths(tree, prefix)?, prefix)? {
            let manifest = manifest(blobs, tree, &format!("{prefix}{id}/package.json"))?;
            manifests.insert(id, manifest);
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

pub fn hook_paths(config: &Value) -> Result<Vec<String>> {
    match config.get("pnpmfile") {
        Some(Value::String(s)) => Ok(vec![relative(s)?]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|s| relative(s.as_str().ok_or("pnpmfile expects paths")?))
            .collect(),
        Some(_) => Err("pnpmfile expects a path or a list of paths".into()),
        None => Ok(vec![".pnpmfile.cjs".into(), ".pnpmfile.mjs".into()]),
    }
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
            if pattern.contains(['{', '}', '(', ')', '\\']) || pattern.starts_with('!') {
                return Err(format!("unsupported workspace glob {value:?}").into());
            }
            let pattern = pattern
                .strip_prefix("./")
                .unwrap_or(pattern)
                .trim_end_matches('/');
            patterns.push(Pattern::new(&format!("{pattern}/package.json"))?);
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
        self.directories(&git::output(&args)?, "")
    }

    pub fn directories(&self, paths: &str, prefix: &str) -> Result<BTreeSet<String>> {
        let mut dirs = BTreeSet::new();
        for path in paths
            .split_terminator('\0')
            .filter_map(|p| p.strip_prefix(prefix))
        {
            let Some((dir, file)) = path.rsplit_once('/') else {
                continue;
            };
            if matches!(file, "package.json" | "package.json5" | "package.yaml")
                && self.contains(dir)
            {
                if file != "package.json" {
                    return Err(format!(
                        "expected a JSON workspace manifest, found {prefix}{path}"
                    )
                    .into());
                }
                dirs.insert(dir.to_owned());
            }
        }
        Ok(dirs)
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
