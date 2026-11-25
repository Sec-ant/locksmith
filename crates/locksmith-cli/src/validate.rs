use std::{collections::BTreeMap, fs, io::ErrorKind, path::Path};

use locksmith::Lockfile;

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
    let project = Project::from_files(lock.recorded_pnpm_version(), read)?;
    if project.config.get("pnpmfileChecksum").is_some()
        || lock
            .project()
            .is_some_and(|doc| doc.get("pnpmfileChecksum").is_some())
    {
        lock.validate()?;
        return Err("workspace validation requires effective manifests when a pnpmfile is active or recorded in the lock".into());
    }
    let config = crate::policy::Policy::from_project(&project).configuration(&project.config)?;
    let mut manifests = BTreeMap::new();
    for id in Members::new(&project)?.directories_on_disk(root)? {
        if let Some(manifest) = workspace::read_manifest(&id, read)? {
            manifests.insert(id, manifest);
        }
    }
    manifests.insert(".".into(), project.root);
    Ok(lock.validate_workspace(&manifests, &config)?)
}
