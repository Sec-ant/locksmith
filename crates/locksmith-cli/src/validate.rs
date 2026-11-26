use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use locksmith::{Lockfile, LockfileError};

use crate::{
    Result,
    workspace::{self, Members, Project},
};

pub fn run(file: &Path, check_workspace: bool) -> Result<()> {
    let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let lock = Lockfile::try_from(text)?;
    if !check_workspace {
        return Ok(lock.validate()?);
    }

    let root = file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let read = |relative: &str| {
        let path = root.join(relative);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display()).into()),
        }
    };
    let read_manifest = |relative: &str| {
        let path = root.join(relative);
        match workspace::read_regular_file(&path) {
            Ok(Some(text)) => Ok(Some(text)),
            Ok(None) => Err(format!("{}: expected a regular manifest file", path.display()).into()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display()).into()),
        }
    };
    let manifest = workspace::read_manifest(".", read_manifest)?;
    let has_root_manifest = manifest.is_some();
    let project = Project::from_manifest(
        manifest.unwrap_or_else(|| serde_json::json!({})),
        lock.recorded_pnpm_version(),
        read,
    )?;
    if !has_root_manifest && !project.workspace {
        return Err("missing root package manifest".into());
    }
    if project.config.get("pnpmfileChecksum").is_some()
        || lock
            .project()
            .is_some_and(|doc| doc.get("pnpmfileChecksum").is_some())
    {
        lock.validate()?;
        return Err("workspace validation requires effective manifests when a pnpmfile is active or recorded in the lock".into());
    }
    let mut config = project.bootstrap_configuration(Some((&lock).into()), || read(".npmrc"))?;
    if let Ok(policy) =
        std::env::var("pnpm_config_pm_on_fail").or_else(|_| std::env::var("PNPM_CONFIG_PM_ON_FAIL"))
    {
        config.pm_on_fail = Some(policy);
    }
    let patches = workspace::patches(&config, "", read)?;
    let canonical_root = root.canonicalize()?;
    let mut directories = Members::directories_on_disk(&project, &canonical_root)?;
    #[cfg(feature = "bench-profile")]
    let manifests_span = locksmith::profile::span("validate_manifest_reads");
    let collect = |directories: BTreeMap<String, PathBuf>| -> Result<_> {
        let mut manifests = BTreeMap::new();
        let mut external_members = BTreeSet::new();
        for (id, directory) in directories {
            if let Some(manifest) = workspace::read_manifest(&id, read_manifest)? {
                if id != ".." && !id.starts_with("../") && !directory.starts_with(&canonical_root) {
                    external_members.insert(id.clone());
                }
                manifests.insert(id, manifest);
            }
        }
        Ok((manifests, external_members))
    };
    let (mut manifests, external_members) = if directories.len() >= 32 {
        let cpus = std::thread::available_parallelism().map_or(2, |cpus| cpus.get());
        let readers = [8, 4]
            .into_iter()
            .find(|&readers| readers <= cpus && readers * 32 <= directories.len())
            .unwrap_or(2);
        let range_size = directories.len().div_ceil(readers);
        // Join adjacent ranges in order so the first manifest error is stable.
        std::thread::scope(|scope| -> Result<_> {
            let mut workers = Vec::with_capacity(readers - 1);
            while directories.len() > range_size {
                let boundary = directories.keys().nth(range_size).unwrap().clone();
                let later = directories.split_off(&boundary);
                let range = std::mem::replace(&mut directories, later);
                workers
                    .push(scope.spawn(move || collect(range).map_err(|error| error.to_string())));
            }
            let last = collect(directories);
            let results = workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .expect("manifest reader panicked")
                        .map_err(Into::into)
                })
                .chain([last]);
            let mut manifests = BTreeMap::new();
            let mut external = BTreeSet::new();
            for result in results {
                let (mut more, mut links) = result?;
                manifests.append(&mut more);
                external.append(&mut links);
            }
            Ok((manifests, external))
        })?
    } else {
        collect(directories)?
    };
    if has_root_manifest {
        manifests.insert(".".into(), project.root);
    }
    #[cfg(feature = "bench-profile")]
    drop(manifests_span);
    let read_linked = |id: &str| {
        let directory = canonical_root.join(id);
        let io = |e| LockfileError::WorkspaceInput(format!("{}: {e}", directory.display()));
        let invalid = || {
            LockfileError::InvalidDocument(format!("no usable linked package manifest at {id:?}"))
        };
        match fs::symlink_metadata(&directory) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(e)),
            Ok(_) => {}
        }
        let file = directory.join("package.json").canonicalize().map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                invalid()
            } else {
                io(e)
            }
        })?;
        if config
            .pnpm_version
            .as_ref()
            .is_some_and(|v| v.major() >= 12)
            && (id == ".." || id.starts_with("../") || !file.starts_with(&canonical_root))
        {
            return Err(invalid());
        }
        let contents = workspace::read_regular_file(&file)
            .map_err(io)?
            .ok_or_else(invalid)?;
        workspace::parse_manifest("package.json", &contents)
            .map(Some)
            .map_err(|_| invalid())
    };
    #[cfg(feature = "bench-profile")]
    let _span = locksmith::profile::span("validate_workspace_graph");
    Ok(lock.validate_workspace(
        &manifests,
        &external_members,
        &config,
        &patches,
        read_linked,
    )?)
}
