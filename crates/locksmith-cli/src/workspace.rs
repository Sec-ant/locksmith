mod patterns;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
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
impl Project {
    pub fn configuration(&self) -> Result<locksmith::WorkspaceConfig> {
        Ok(crate::policy::Policy::from_project(self).configuration(&self.config)?)
    }

    pub fn bootstrap_configuration(
        &self,
        lock: Option<locksmith::MergeInput<'_>>,
        read_rc: impl FnOnce() -> Result<Option<String>>,
    ) -> Result<locksmith::WorkspaceConfig> {
        let mut config = self.configuration()?;
        config
            .config_registries
            .extend(crate::registry::bootstrap_registries(
                &self.config,
                lock,
                read_rc,
            )?);
        Ok(config)
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
    let mut config = match root.get("pnpm").filter(|_| legacy) {
        Some(v) if v.is_object() => v.clone(),
        Some(_) => {
            return Err("package.json: expected pnpm to be an object".into());
        }
        None => json!({}),
    };
    // Membership is declared in pnpm-workspace.yaml, including the omitted-key default.
    config.as_object_mut().unwrap().remove("packages");
    // Read this hook setting only from the configuration files supported by pnpm.
    config.as_object_mut().unwrap().remove("ignorePnpmfile");
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
            if version.is_some_and(|v| v < &node_semver::Version::from((10, 5, 0)))
                && !settings.is_empty()
                && !settings.contains_key("packages")
            {
                return Err("pnpm-workspace.yaml: this pnpm version requires packages in a nonempty workspace configuration".into());
            }
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
    #[cfg(feature = "bench-profile")]
    let _span = locksmith::profile::span("committed_manifests");
    let mut manifests = BTreeMap::from([(".".into(), project.root.clone())]);
    if project.workspace {
        let members = Members::new(project)?;
        let mut paths = BTreeMap::new();
        let root = format!("{tree}:{}", prefix.trim_end_matches('/'));
        members.committed_paths(&root, "", tree, prefix, &mut paths, blobs)?;
        git::prefetch_manifests(paths.values().map(|(_, id)| id.as_str()))?;
        if !blobs.conflicts.is_empty() {
            blobs.check_linked_conflicts(
                tree,
                paths
                    .iter()
                    .map(|(id, (name, _))| format!("{prefix}{id}/{name}")),
            )?;
        }
        for (id, (name, oid)) in paths {
            let text = blobs
                .read_object(&oid)?
                .ok_or_else(|| format!("missing manifest at {tree}:{prefix}{id}/{name}"))?;
            let manifest = parse_manifest(&format!("{prefix}{id}/{name}"), &text)?;
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
    descend: BTreeMap<String, Vec<Pattern>>,
    excluded_trees: Vec<Pattern>,
    component_patterns: BTreeMap<String, Pattern>,
}
impl Members {
    pub fn new(project: &Project) -> Result<Self> {
        Self::from_patterns(project, false)
    }

    fn from_patterns(project: &Project, on_disk: bool) -> Result<Self> {
        let mut this = Self {
            include: vec![],
            exclude: vec![],
            descend: BTreeMap::new(),
            excluded_trees: Vec::new(),
            component_patterns: BTreeMap::new(),
        };
        if !project.workspace {
            return Ok(this);
        }
        let patterns: Vec<String> = match project.config.get("packages") {
            None if project
                .pnpm_version
                .as_ref()
                .is_some_and(|v| v < &node_semver::Version::from((10, 5, 0))) =>
            {
                vec!["**".into()]
            }
            None => Vec::new(),
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
            for alternative in patterns::expand(
                pattern,
                value.starts_with('!'),
                project.pnpm_version.as_ref(),
                on_disk,
            )? {
                let parts = patterns::components(&alternative);
                if !parts.is_empty() {
                    for &part in &parts {
                        if !this.component_patterns.contains_key(part) {
                            this.component_patterns
                                .insert(part.into(), Pattern::new(part)?);
                        }
                    }
                    patterns.push(Pattern::new(&format!("{}/package.json", parts.join("/")))?);
                    if !value.starts_with('!') {
                        let parents = parts.iter().take_while(|&&p| p == "..").count();
                        let descend = this.descend.entry(parts[..parents].join("/")).or_default();
                        for (index, part) in parts.iter().enumerate().skip(parents) {
                            if index + 1 < parts.len() || *part == "**" {
                                descend.push(Pattern::new(&parts[..=index].join("/"))?);
                            }
                        }
                    } else if let Some(prefix) = parts.strip_suffix(&["**"]) {
                        let tree = if prefix.is_empty() {
                            "**".to_owned()
                        } else {
                            prefix.join("/")
                        };
                        this.excluded_trees.push(Pattern::new(&tree)?);
                    }
                }
            }
        }
        Ok(this)
    }

    pub fn working_directories(&self, prefix: &str) -> Result<BTreeSet<String>> {
        #[cfg(feature = "bench-profile")]
        let _span = locksmith::profile::span("worktree_member_discovery");
        let mut directories = BTreeSet::new();
        if self.include.is_empty() {
            return Ok(directories);
        }
        let mut args = vec![
            "ls-files",
            "-zcv",
            "--stage",
            "--others",
            "--exclude-standard",
        ];
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
        // An indexed regular file may have become a directory symlink in the worktree.
        let (paths, changed) = std::thread::scope(|scope| -> Result<_> {
            let changed = scope.spawn(|| {
                git::output_bytes(&[
                    "diff-files",
                    "--relative",
                    "--name-only",
                    "-z",
                    "--diff-filter=T",
                    "--",
                    ".",
                ])
                .map_err(|error| error.to_string())
            });
            let paths = git::output_bytes(&args)?;
            Ok((
                paths,
                changed.join().expect("Git type change scan panicked")?,
            ))
        })?;
        let changed: BTreeSet<_> = changed.split(|&b| b == 0).collect();
        let root = std::env::current_dir()?.canonicalize()?;
        let descend = self.descend.get("").map_or(&[][..], Vec::as_slice);
        for record in paths.split(|&b| b == 0).filter(|p| !p.is_empty()) {
            let (path, possible_link) = if let Some(path) = record.strip_prefix(b"? ") {
                (path, true)
            } else {
                let tab = record
                    .iter()
                    .position(|&b| b == b'\t')
                    .ok_or("invalid git ls-files entry")?;
                let path = &record[tab + 1..];
                (
                    path,
                    record.get(2..8) == Some(b"120000")
                        || changed.contains(path)
                        || record[0].is_ascii_lowercase()
                        || record[0] == b'S',
                )
            };
            let Ok(path) = std::str::from_utf8(path) else {
                continue;
            };
            if let Some((id, name)) = path.rsplit_once('/')
                && MANIFEST_NAMES.contains(&name)
                && self.contains(id)
            {
                directories.insert(id.to_owned());
            }
            // Git lists a directory symlink itself, never its member manifests.
            if !possible_link || !self.may_visit(path, descend) {
                continue;
            }
            let link = root.join(path);
            match fs::symlink_metadata(&link) {
                Ok(meta) if meta.file_type().is_symlink() => {}
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
            let Some(target) = linked_directory(&link)? else {
                continue;
            };
            let mut ancestors: BTreeSet<_> = link.ancestors().skip(1).map(Path::to_owned).collect();
            if ancestors.insert(target.clone()) {
                self.visit(&target, path, descend, &mut ancestors, &mut |id, _| {
                    directories.insert(id.to_owned());
                })?;
            }
        }
        Ok(directories)
    }

    /// Discover manifests from disk, including projects absent from Git or the lockfile.
    pub fn directories_on_disk(
        project: &Project,
        root: &Path,
    ) -> Result<BTreeMap<String, PathBuf>> {
        #[cfg(feature = "bench-profile")]
        let _span = locksmith::profile::span("disk_member_discovery");
        let members = Self::from_patterns(project, true)?;
        let mut found = BTreeMap::new();
        for (id, descend) in &members.descend {
            let path = root.join(id).canonicalize()?;
            let mut ancestors = BTreeSet::from([path.clone()]);
            members.visit(&path, id, descend, &mut ancestors, &mut |id, directory| {
                let id = importer_id(root, id);
                if !id.is_empty() {
                    found.insert(id, directory.to_owned());
                }
            })?;
        }
        Ok(found)
    }

    fn visit(
        &self,
        directory: &Path,
        id: &str,
        descend: &[Pattern],
        ancestors: &mut BTreeSet<PathBuf>,
        found: &mut impl FnMut(&str, &Path),
    ) -> Result<()> {
        let entries =
            fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if MANIFEST_NAMES.contains(&name) && !id.is_empty() && self.contains(id) {
                found(id, directory);
            }
            if matches!(name, ".git" | "node_modules" | "bower_components") {
                continue;
            }
            let child = if id.is_empty() {
                name.to_owned()
            } else {
                format!("{id}/{name}")
            };
            let kind = entry.file_type()?;
            if !kind.is_dir() && !kind.is_symlink() {
                continue;
            }
            if !self.may_visit(&child, descend) {
                continue;
            }
            let path = if kind.is_symlink() {
                let Some(target) = linked_directory(&entry.path())? else {
                    continue;
                };
                target
            } else if kind.is_dir() {
                entry.path()
            } else {
                continue;
            };
            // Detect cycles on this branch; two aliases may both be genuine importer IDs.
            if ancestors.insert(path.clone()) {
                self.visit(&path, &child, descend, ancestors, found)?;
                ancestors.remove(&path);
            }
        }
        Ok(())
    }

    fn committed_paths(
        &self,
        object: &str,
        alias: &str,
        tree: &str,
        prefix: &str,
        found: &mut BTreeMap<String, (String, String)>,
        blobs: &mut Blobs,
    ) -> Result<()> {
        let listing = git::output_bytes(&["ls-tree", "-rz", "--full-tree", object])?;
        let mut empty_links = BTreeSet::new();
        for entry in git::entries(&listing) {
            let (kind, oid, path) = entry?;
            if !matches!(kind, git::ObjectKind::Blob | git::ObjectKind::Symlink) {
                continue;
            }
            let Ok(path) = std::str::from_utf8(path) else {
                continue;
            };
            if kind != git::ObjectKind::Symlink
                && !MANIFEST_NAMES.contains(&path.rsplit('/').next().expect("path component"))
            {
                continue;
            }
            let logical = if alias.is_empty() {
                path.to_owned()
            } else {
                format!("{alias}/{path}")
            };
            let manifest = logical
                .rsplit_once('/')
                .filter(|(id, name)| MANIFEST_NAMES.contains(name) && self.contains(id));
            let manifest = manifest.filter(|(id, name)| {
                found
                    .get(*id)
                    .is_none_or(|(preferred, _)| *name < preferred.as_str())
            });
            if kind == git::ObjectKind::Symlink {
                let descend = self.descend.get("").map_or(&[][..], Vec::as_slice);
                let directory = self.may_visit(&logical, descend);
                if manifest.is_none() && !directory {
                    continue;
                }
                let Some(target) = blobs.linked_object(tree, prefix, &logical)? else {
                    continue;
                };
                if target.kind == git::ObjectKind::Tree && directory {
                    // Sibling aliases with equivalent glob matches have identical descendants.
                    // Reuse only empty matches: nonempty aliases each have their own importer IDs.
                    let (parent, name) = logical.rsplit_once('/').unwrap_or(("", &logical));
                    let pattern_matches: Vec<_> = self
                        .component_patterns
                        .values()
                        .map(|pattern| pattern.matches_with(name, GLOB_OPTIONS))
                        .collect();
                    let cache_key = (
                        parent.to_owned(),
                        target.physical_path,
                        target.links,
                        name.starts_with('.'),
                        pattern_matches,
                    );
                    if empty_links.contains(&cache_key) {
                        continue;
                    }
                    let start = found.len();
                    self.committed_paths(&target.id, &logical, tree, prefix, found, blobs)?;
                    if found.len() == start && blobs.conflicts.is_empty() {
                        empty_links.insert(cache_key);
                    }
                } else if target.kind == git::ObjectKind::Blob
                    && let Some((id, name)) = manifest
                {
                    Self::record_manifest(found, id, name, &target.id);
                }
            } else if let Some((id, name)) = manifest {
                Self::record_manifest(found, id, name, oid);
            }
        }
        Ok(())
    }

    fn record_manifest(
        found: &mut BTreeMap<String, (String, String)>,
        id: &str,
        name: &str,
        oid: &str,
    ) {
        let record = found
            .entry(id.to_owned())
            .or_insert_with(|| (name.to_owned(), oid.to_owned()));
        if name < record.0.as_str() {
            *record = (name.to_owned(), oid.to_owned());
        }
    }

    fn may_visit(&self, path: &str, descend: &[Pattern]) -> bool {
        let matches = |p: &Pattern| p.matches_with(path, GLOB_OPTIONS);
        !excluded_directory(path)
            && !self.excluded_trees.iter().any(matches)
            && (descend.iter().any(matches) || self.contains(path))
    }

    pub fn contains(&self, dir: &str) -> bool {
        if excluded_directory(dir) {
            return false;
        }
        let path = format!("{dir}/package.json");
        let matches = |p: &Pattern| p.matches_with(&path, GLOB_OPTIONS);
        self.include.iter().any(matches) && !self.exclude.iter().any(matches)
    }
}

fn excluded_directory(path: &str) -> bool {
    path.split('/')
        .any(|p| matches!(p, ".git" | "node_modules" | "bower_components"))
}

fn linked_directory(path: &Path) -> Result<Option<PathBuf>> {
    match path.canonicalize() {
        Ok(target) => Ok(fs::metadata(&target)?.is_dir().then_some(target)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Ok(None),
        #[cfg(windows)]
        Err(e) if e.raw_os_error() == Some(1921) => Ok(None), // ERROR_CANT_RESOLVE_FILENAME
        Err(e) => Err(format!("{}: {e}", path.display()).into()),
    }
}

/// Normalize a parent-relative glob match without resolving symlinks in its importer ID.
fn importer_id(root: &Path, path: &str) -> String {
    let mut parts = path.split('/').peekable();
    let mut base = root;
    while parts.next_if_eq(&"..").is_some() {
        base = base.parent().unwrap_or(base);
    }
    let mut from = root
        .strip_prefix(base)
        .expect("workspace ancestor")
        .components()
        .peekable();
    while from
        .peek()
        .is_some_and(|part| parts.peek().is_some_and(|name| part.as_os_str() == *name))
    {
        from.next();
        parts.next();
    }
    std::iter::repeat_n("..", from.count())
        .chain(parts)
        .collect::<Vec<_>>()
        .join("/")
}
