//! Non-interactive Git low-level merge driver and setup helper for pnpm lockfiles.
use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    io::{self, Write},
    path::Path,
    process::Command,
};

use locksmith::MergeContext;
use tempfile::NamedTempFile;

use crate::{
    ExitStatus, Result,
    args::DriverSetupArgs,
    git,
    ui::{Ui, write_atomic},
    workspace,
};

use super::merge;
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

/// Quote a string as a single POSIX shell token, leaving plain safe words unquoted.
fn shell_quote_token(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'))
    {
        return value.to_string();
    }
    shell_quote_single(value)
}

/// Always quote a string using POSIX single-quote escaping (`'` -> `'\''`).
fn shell_quote_single(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Escape a leading `!` at the pattern layer and apply Git's C-style quoting when needed.
fn quote_gitattributes_pattern(pattern: &str) -> String {
    let needs_quoting = pattern.starts_with('#')
        || pattern
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\\'));
    let escaped = if let Some(rest) = pattern.strip_prefix('!') {
        format!("\\!{rest}")
    } else {
        pattern.to_string()
    };
    if !needs_quoting {
        return escaped;
    }
    let mut out = String::with_capacity(escaped.len() + 2);
    out.push('"');
    for ch in escaped.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn detect_local_driver_bin() -> Option<String> {
    let local = Path::new("node_modules/.bin/locksmith");
    if !local.exists() {
        return None;
    }
    let cwd = env::current_dir().ok()?.canonicalize().ok()?;
    let full = cwd.join(local);
    if let Ok(top) = git::output(&["rev-parse", "--show-toplevel"])
        && let Ok(top_canon) = Path::new(top.trim()).canonicalize()
        && let Ok(rel) = full.strip_prefix(&top_canon)
    {
        let rel = rel.to_string_lossy().replace('\\', "/");
        return Some(format!("./{rel}"));
    }
    Some(full.to_string_lossy().into_owned())
}

pub fn setup(args: &DriverSetupArgs, ui: &Ui) -> Result<()> {
    let bin = match &args.bin {
        Some(bin) => bin.clone(),
        None if !args.global => detect_local_driver_bin().unwrap_or_else(|| "locksmith".into()),
        None => "locksmith".to_string(),
    };
    let escaped_bin = shell_quote_token(&bin.replace('%', "%%"));
    let network = if args.allow_network {
        " --allow-network"
    } else {
        ""
    };
    let driver_cmd = format!("{escaped_bin} driver run %O %A %B %P{network}");
    let quoted_pattern = quote_gitattributes_pattern(&args.lockfile);
    let gitattributes_line = format!("{quoted_pattern} merge=locksmith");
    let scope_flag = if args.global { "--global" } else { "--local" };

    if args.apply {
        let name_status = Command::new("git")
            .args([
                "config",
                scope_flag,
                "merge.locksmith.name",
                "locksmith pnpm lockfile merge driver",
            ])
            .status()?;
        if !name_status.success() {
            return Err("failed to write merge.locksmith.name to git config".into());
        }
        let driver_status = Command::new("git")
            .args(["config", scope_flag, "merge.locksmith.driver", &driver_cmd])
            .status()?;
        if !driver_status.success() {
            return Err("failed to write merge.locksmith.driver to git config".into());
        }
        let mut out = io::stdout().lock();
        writeln!(out, "Configured git ({scope_flag}):")?;
        writeln!(out, "  merge.locksmith.driver = {driver_cmd}")?;
        writeln!(out, "\nEnsure .gitattributes contains:")?;
        writeln!(out, "  {gitattributes_line}")?;
        return Ok(());
    }

    let mut out = io::stdout().lock();
    writeln!(out, "# 1. Add to .gitattributes:")?;
    writeln!(out, "{gitattributes_line}")?;
    writeln!(out)?;
    writeln!(out, "# 2. Register the Git merge driver:")?;
    writeln!(
        out,
        "git config {scope_flag} merge.locksmith.name 'locksmith pnpm lockfile merge driver'"
    )?;
    writeln!(
        out,
        "git config {scope_flag} merge.locksmith.driver {}",
        shell_quote_single(&driver_cmd)
    )?;
    ui.hint("pass --apply to write the git config entries automatically");
    Ok(())
}

pub fn run(files: [&OsString; 3], path: &str, allow_network: bool, ui: &Ui) -> Result<()> {
    let prefix = path
        .strip_suffix("pnpm-lock.yaml")
        .filter(|p| p.is_empty() || p.ends_with('/'))
        .ok_or("expected the Git path of pnpm-lock.yaml")?;
    let destination = Path::new(files[1]);
    match resolve(files, path, prefix, allow_network, ui) {
        Ok(yaml) => {
            write_atomic(destination, yaml.as_bytes())?;
            Ok(())
        }
        Err(reason) => {
            ui.print_error(&*reason);
            let dir = destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let temp = NamedTempFile::new_in(dir)?;
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
            temp.persist(destination).map_err(|e| e.error)?;
            ui.hint(format!("{path}: {message}"));
            Err(Box::new(ExitStatus(1)))
        }
    }
}

fn resolve(
    files: [&OsString; 3],
    path: &str,
    prefix: &str,
    allow_network: bool,
    ui: &Ui,
) -> Result<String> {
    let inputs = git::revisions(files, path)?;
    let revisions = &inputs.revisions;
    let (tree, conflicts) = git::merged_tree(&inputs)?;
    let mut blobs = git::Blobs::open(workspace::CONFIG_FILES, &workspace::DEFAULT_HOOK_FILES)?;
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
    let locks = merge::Sources::read(files)?;
    let target = git::preferred_target(&inputs)?;
    ui.verbose(format!(
        "git merge driver resolving {path} (preferred target: {})",
        match target {
            locksmith::MergeTarget::Ours => "ours",
            locksmith::MergeTarget::Theirs => "theirs",
        }
    ));
    let mut read = |path: &str| blobs.read(&tree, &format!("{prefix}{path}"));
    let (root_name, root) =
        workspace::read_named_manifest(".", &mut read)?.ok_or("missing root package manifest")?;
    let (project, config, merge) =
        target.workspace_for_candidate(Some(&root), locks.inputs(), |lock| -> Result<_> {
            let version = lock.and_then(locksmith::Lockfile::recorded_pnpm_version);
            let project = Project::from_manifest(root.clone(), version, &mut read)?;
            let config =
                project.bootstrap_configuration(lock.map(Into::into), || read(".npmrc"))?;
            Ok((project, config))
        })?;
    let dependency_inputs = project_conflicts(&conflicts, prefix, &[&project])?;
    let manifests = workspace::manifests(&project, &mut blobs, &tree, prefix)?;
    if !blobs.conflicts.is_empty() {
        blobs.check_linked_conflicts(
            &tree,
            dependency_inputs
                .iter()
                .filter(|p| {
                    p.as_str() == root_name || !workspace::MANIFEST_NAMES.contains(&p.as_str())
                })
                .map(|p| format!("{prefix}{p}")),
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
        target,
        manifests: &manifests,
        configurations: configurations.each_ref(),
        configuration: &config,
        patches: &patches,
        registry: None,
    };
    let network = allow_network.then_some(crate::registry::ProjectRc::Git {
        settings: settings.as_deref(),
        credentials: credentials.as_deref(),
    });
    let result = merge::with_context(&merge, &context, &project.config, network, ui)?;
    Ok(result.to_yaml()?)
}
