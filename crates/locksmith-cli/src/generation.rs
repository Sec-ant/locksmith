use std::{
    io::{self, Write},
    path::Path,
};

use locksmith::{GenerateContext, Lockfile, WorkspaceConfig};
use node_semver::Version;

pub mod hooks;
use crate::{
    Result,
    registry::GenerationSource,
    workspace::{self, Members, Project, working_file},
};

pub fn run(
    allow_network: bool,
    stdout: bool,
    explicit: Option<Version>,
    runtime: hooks::Runtime,
) -> Result<()> {
    let previous = working_file("pnpm-lock.yaml")?
        .ok_or("generate requires an existing pnpm-lock.yaml in the workspace root")?;
    let old = Lockfile::try_from(previous.clone())?;
    let root =
        workspace::read_manifest(".", working_file)?.ok_or("missing root package manifest")?;
    let declared = workspace::pnpm_version(&root);
    if let Some(value) = root["packageManager"].as_str()
        && declared.is_none()
        && explicit.is_none()
    {
        return Err(format!(
            "packageManager {value:?} does not specify an exact pnpm version; supply --pnpm-version"
        )
        .into());
    }
    let selected = explicit.or(declared);
    if let Some(v) = &selected
        && !(9..=12).contains(&v.major())
    {
        return Err(format!("expected pnpm 9 through 12, got {v}").into());
    }
    let version = selected.or_else(|| old.recorded_pnpm_version());
    let mut project = Project::from_files_with_version(root, version.as_ref(), working_file)?;
    project.config["lockfileDir"] = std::env::current_dir()?
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
        .into();
    let mut hooks = hooks::Hooks::open(runtime, &mut project.config, version.as_ref())?;
    if project.config["sharedWorkspaceLockfile"] == false {
        return Err("generate requires a shared workspace lockfile".into());
    }
    let config = match &version {
        Some(v) => WorkspaceConfig::from_pnpm_version(&project.config, v)?,
        None => WorkspaceConfig::from_pnpm(&project.config)?,
    };
    let patches = workspace::patches(&config, "", working_file)?;
    let manifests =
        Members::for_generation(&project, version.as_ref())?.filesystem_manifests(&project.root)?;
    let mut input = GenerateContext::new(&manifests, &project.config).with_patches(&patches);
    if let Some(version) = &version {
        input = input.with_pnpm_version(version);
    }
    let mut registry = GenerationSource::new(
        &project.config,
        version.as_ref(),
        allow_network,
        hooks.as_ref().is_some_and(hooks::Hooks::reads_packages),
    );
    let result = match &mut hooks {
        Some(hooks) => locksmith::generate_with_hooks(&old, &input, &mut registry, hooks)?,
        None => locksmith::generate(&old, &input, &mut registry)?,
    };
    let text = match project.config["lockfile"]["format"].as_str() {
        None | Some("v9") => result.to_yaml()?,
        Some(format) => {
            return Err(format!("lockfile.format {format} is not supported").into());
        }
    };
    if stdout {
        io::stdout().lock().write_all(text.as_bytes())?;
    } else {
        let path = Path::new("pnpm-lock.yaml");
        let replacement = if previous != text {
            // Preserve the lockfile mode without exposing the partial contents.
            let mut file = tempfile::NamedTempFile::new_in(".")?;
            file.write_all(text.as_bytes())?;
            file.as_file()
                .set_permissions(path.metadata()?.permissions())?;
            Some(file)
        } else {
            None
        };
        // Hooks and registry requests can take long enough for another writer
        // to replace the lockfile. Refuse to overwrite a lock this run did not read.
        if working_file("pnpm-lock.yaml")?.as_deref() != Some(previous.as_str()) {
            return Err("pnpm-lock.yaml changed during generation; run generate again".into());
        }
        if let Some(file) = replacement {
            file.persist(path)?;
        }
    }
    Ok(())
}
