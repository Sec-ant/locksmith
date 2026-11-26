use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::Path,
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
    let manifest = workspace::read_manifest(".", read)?;
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
    let config = project.bootstrap_configuration(Some((&lock).into()), || read(".npmrc"))?;
    let patches = workspace::patches(&config, "", read)?;
    let mut manifests = BTreeMap::new();
    let canonical_root = root.canonicalize()?;
    let mut external_members = BTreeSet::new();
    for (id, directory) in Members::directories_on_disk(&project, &canonical_root)? {
        if let Some(manifest) = workspace::read_manifest(&id, read)? {
            if id != ".." && !id.starts_with("../") && !directory.starts_with(&canonical_root) {
                external_members.insert(id.clone());
            }
            manifests.insert(id, manifest);
        }
    }
    if has_root_manifest {
        manifests.insert(".".into(), project.root);
    }
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
        if !fs::metadata(&file).map_err(io)?.is_file() {
            return Err(invalid());
        }
        let contents = fs::read_to_string(&file).map_err(io)?;
        workspace::parse_manifest("package.json", &contents)
            .map(Some)
            .map_err(|_| invalid())
    };
    Ok(lock.validate_workspace(
        &manifests,
        &external_members,
        &config,
        &patches,
        read_linked,
    )?)
}
