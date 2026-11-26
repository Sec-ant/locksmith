//! Non-interactive Git low-level merge driver for pnpm lockfiles.
use std::{collections::BTreeSet, ffi::OsString, io::Write, path::Path, process::Command};

use locksmith::MergeContext;
use tempfile::NamedTempFile;

use crate::{Result, git, workspace};
use workspace::{Members, Project};

fn project_conflicts(
    conflicts: &BTreeSet<Vec<u8>>,
    prefix: &str,
    projects: &[&Project],
) -> Result<BTreeSet<String>> {
    let mut inputs = BTreeSet::new();
    for name in workspace::CONFIG_FILES {
        inputs.insert(name.to_string());
    }
    let mut workspaces = Vec::new();
    for project in projects {
        workspaces
            .push(Members::new(project).map_err(|e| format!("{prefix}pnpm-workspace.yaml: {e}"))?);
        if project.config["ignorePnpmfile"] != true {
            inputs.extend(workspace::DEFAULT_HOOK_FILES.into_iter().map(str::to_owned));
        }
        inputs.extend(workspace::file_inputs(&project.config)?);
    }
    workspace::check_conflicts(conflicts, |path| {
        let Some(path) = path.strip_prefix(prefix) else {
            return false;
        };
        inputs.contains(path)
            || path.rsplit_once('/').is_some_and(|(dir, name)| {
                workspace::MANIFEST_NAMES.contains(&name)
                    && workspaces.iter().any(|w| w.contains(dir))
            })
    })?;
    Ok(inputs)
}

pub fn run(files: [&OsString; 3], path: &str, allow_network: bool) -> Result<()> {
    let prefix = path
        .strip_suffix("pnpm-lock.yaml")
        .filter(|p| p.is_empty() || p.ends_with('/'))
        .ok_or("expected the Git path of pnpm-lock.yaml")?;
    let destination = Path::new(files[1]);
    let mut temp = NamedTempFile::new_in(destination.parent().unwrap_or(Path::new(".")))?;
    match resolve(files, path, prefix, allow_network) {
        Ok(yaml) => {
            temp.write_all(yaml.as_bytes())?;
            temp.persist(destination)?;
            Ok(())
        }
        Err(reason) => {
            eprintln!("locksmith: {reason}");
            let [base, ours, theirs] = files;
            let result = Command::new("git")
                .args([
                    "merge-file",
                    "-p",
                    "-L",
                    "ours",
                    "-L",
                    "base",
                    "-L",
                    "theirs",
                    "--",
                ])
                .args([ours, base, theirs])
                .stdout(temp.reopen()?)
                .output()?;
            let message = match result.status.code() {
                Some(0) => {
                    "text merged cleanly; review the unresolved dependency or configuration before staging"
                }
                Some(1..=127) => "resolve the conflict markers before staging",
                _ => {
                    return Err(format!(
                        "git merge-file failed: {}",
                        String::from_utf8_lossy(&result.stderr).trim()
                    )
                    .into());
                }
            };
            temp.persist(destination)?;
            Err(format!("{path}: {message}").into())
        }
    }
}

fn resolve(files: [&OsString; 3], path: &str, prefix: &str, allow_network: bool) -> Result<String> {
    let inputs = git::revisions(files, path)?;
    let revisions = &inputs.revisions;
    let (tree, conflicts) = git::merged_tree(&inputs)?;
    let mut blobs = git::Blobs::open()?;
    let [base, ours, theirs] = revisions
        .each_ref()
        .map(|r| Project::read(&mut blobs, r, prefix, None));
    let sources = [base?, ours?, theirs?];
    let root_conflict = workspace::CONFIG_FILES
        .iter()
        .any(|p| conflicts.contains(format!("{prefix}{p}").as_bytes()));
    if root_conflict {
        project_conflicts(&conflicts, prefix, &sources.each_ref())?;
    }
    blobs.conflicts = conflicts
        .iter()
        .filter(|p| p.as_slice() != path.as_bytes())
        .cloned()
        .collect();
    let locks = crate::merge::Sources::read(files)?;
    let target = git::preferred_target(&inputs)?;
    let mut read = |path: &str| blobs.read(&tree, &format!("{prefix}{path}"));
    let root = workspace::read_manifest(".", &mut read)?.ok_or("missing root package manifest")?;
    let (project, config, merge) =
        target.workspace_for_bootstrap(Some(&root), locks.inputs(), |version| -> Result<_> {
            let project = Project::from_manifest(root.clone(), version, &mut read)?;
            let config = project.configuration()?;
            Ok((project, config))
        })?;
    let dependency_inputs = project_conflicts(&conflicts, prefix, &[&project])?;
    let manifests = workspace::manifests(&project, &mut blobs, &tree, prefix)?;
    if !blobs.conflicts.is_empty() {
        blobs.check_linked_conflicts(
            &tree,
            dependency_inputs.iter().map(|p| format!("{prefix}{p}")),
        )?;
    }
    let [base, ours, theirs] = sources.each_ref().map(|p| p.configuration());
    let configurations = [base?, ours?, theirs?];
    let patches = workspace::patches(&config, prefix, |file| blobs.read(&tree, file))?;
    let mut settings = None;
    let mut credentials = None;
    if allow_network {
        let path = format!("{prefix}.npmrc");
        settings = blobs.read(&tree, &path)?;
        let side = match target {
            locksmith::MergeTarget::Ours => 1,
            locksmith::MergeTarget::Theirs => 2,
        };
        // Credentials come from the preferred commit, whose paths have no preview conflicts.
        blobs.conflicts.clear();
        credentials = blobs.read(&revisions[side], &path)?;
    }
    drop(blobs);
    let context = MergeContext {
        registry: None,
        target,
        manifests: &manifests,
        configuration: &config,
        configurations: configurations.each_ref(),
        patches: &patches,
    };
    let network = allow_network.then_some(crate::registry::ProjectRc::Git {
        settings: settings.as_deref(),
        credentials: credentials.as_deref(),
    });
    let result = crate::merge::with_context(&merge, &context, &project.config, network)?;
    Ok(result.to_yaml()?)
}
