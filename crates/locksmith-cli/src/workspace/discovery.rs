//! Discover workspace members in the worktree, on disk, or in a committed Git tree.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::BufRead,
    path::{Path, PathBuf},
};

use glob::{MatchOptions, Pattern};
use serde_json::Value;

use crate::{
    Result,
    git::{self, Blobs},
};

use super::{MANIFEST_NAMES, Project, parse_manifest, patterns};

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
        if members.include.is_empty() {
            return Ok(manifests);
        }
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
        let root = std::env::current_dir()?.canonicalize()?;
        let descend = self.descend.get("").map_or(&[][..], Vec::as_slice);
        git::stream_paths(&args, |paths| {
            // An indexed regular file may have become a directory symlink in the worktree.
            let changed = git::output_bytes(&[
                "diff-files",
                "--relative",
                "--name-only",
                "-z",
                "--diff-filter=T",
                "--",
                ".",
            ])?;
            let changed: BTreeSet<_> = changed.split(|&b| b == 0).collect();
            let mut record = Vec::new();
            loop {
                record.clear();
                if paths.read_until(0, &mut record)? == 0 {
                    break;
                }
                let record = record.strip_suffix(b"\0").unwrap_or(&record);
                if record.is_empty() {
                    continue;
                }
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
                let mut ancestors: BTreeSet<_> =
                    link.ancestors().skip(1).map(Path::to_owned).collect();
                if ancestors.insert(target.clone()) {
                    self.visit(
                        &target,
                        path,
                        descend,
                        &mut ancestors,
                        false,
                        &mut |id, _| {
                            directories.insert(id.to_owned());
                        },
                    )?;
                }
            }
            Ok(())
        })?;
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
            members.visit(
                &path,
                id,
                descend,
                &mut ancestors,
                true,
                &mut |id, directory| {
                    let id = importer_id(root, id);
                    if !id.is_empty() {
                        found.insert(id, directory.to_owned());
                    }
                },
            )?;
        }
        Ok(found)
    }

    fn visit(
        &self,
        directory: &Path,
        id: &str,
        descend: &[Pattern],
        ancestors: &mut BTreeSet<PathBuf>,
        split: bool,
        found: &mut dyn FnMut(&str, &Path),
    ) -> Result<()> {
        let entries =
            fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        if !split {
            return self.visit_entries(entries, id, descend, ancestors, split, found);
        }
        let mut entries: Vec<_> = entries.collect();
        if entries.len() < 32 {
            return self.visit_entries(entries, id, descend, ancestors, split, found);
        }
        let readers = if entries.len() >= 128
            && std::thread::available_parallelism().is_ok_and(|cpus| cpus.get() >= 4)
        {
            4
        } else {
            2
        };
        let range_size = entries.len().div_ceil(readers);
        let collect = |entries, ancestors: &mut BTreeSet<PathBuf>| -> Result<_> {
            let mut discovered = Vec::new();
            self.visit_entries(
                entries,
                id,
                descend,
                ancestors,
                false,
                &mut |id, directory| discovered.push((id.to_owned(), directory.to_owned())),
            )?;
            Ok(discovered)
        };
        std::thread::scope(|scope| -> Result<()> {
            let mut workers = Vec::with_capacity(readers - 1);
            while entries.len() > range_size {
                let later = entries.split_off(range_size);
                let range = std::mem::replace(&mut entries, later);
                let mut path = ancestors.clone();
                workers.push(
                    scope.spawn(move || {
                        collect(range, &mut path).map_err(|error| error.to_string())
                    }),
                );
            }
            let last = collect(entries, ancestors);
            let results = workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .expect("workspace directory reader panicked")
                        .map_err(Into::into)
                })
                .chain([last]);
            // Apply discoveries in directory iteration order.
            for result in results {
                for (id, path) in result? {
                    found(&id, &path);
                }
            }
            Ok(())
        })
    }

    fn visit_entries(
        &self,
        entries: impl IntoIterator<Item = std::io::Result<fs::DirEntry>>,
        id: &str,
        descend: &[Pattern],
        ancestors: &mut BTreeSet<PathBuf>,
        split: bool,
        found: &mut dyn FnMut(&str, &Path),
    ) -> Result<()> {
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if MANIFEST_NAMES.contains(&name) && !id.is_empty() && self.contains(id) {
                found(
                    id,
                    entry.path().parent().expect("directory entry has a parent"),
                );
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
                self.visit(&path, &child, descend, ancestors, split, found)?;
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
