//! Select a complete pnpm bootstrap graph and interpret the project with its runner.
use std::borrow::Cow;

use node_semver::Version;

use super::{MergeTarget, Source, missing};
use crate::{
    ContextualMerge, Lockfile, LockfileError, MergeInput, RepairInput, Value, WorkspaceConfig,
    manifest::ManagerPolicy,
};

impl MergeTarget {
    /// Decode each branch with its own bootstrap version and the final settings
    /// with the version from the selected bootstrap graph. Use the returned merge with these settings.
    ///
    /// # Errors
    /// Returns an error for invalid settings or when no bootstrap graph matches the final workspace.
    pub fn workspace_configurations<'a>(
        self,
        manifest: Option<&Value>,
        inputs: [MergeInput<'a>; 3],
        branches: [&Value; 3],
        final_settings: &Value,
    ) -> Result<([WorkspaceConfig; 3], WorkspaceConfig, ContextualMerge<'a>), LockfileError> {
        let bootstrap = bootstrap_locks(inputs);
        let locks = bootstrap.each_ref().map(|lock| lock.as_deref());
        let [base, ours, theirs] = [0, 1, 2].map(|i| {
            let version = locks[i].and_then(Lockfile::recorded_pnpm_version);
            WorkspaceConfig::from_pnpm_source(branches[i], manifest, version)
        });
        let ((), config, bootstrap) = select_workspace(self, manifest, locks, |version| {
            WorkspaceConfig::from_pnpm_context(final_settings, manifest, version)
                .map(|config| ((), config))
        })?;
        Ok((
            [base?, ours?, theirs?],
            config,
            ContextualMerge {
                inputs,
                bootstrap: Some(bootstrap),
            },
        ))
    }

    /// Load a filesystem workspace using the pnpm version from the selected bootstrap graph.
    /// The loader receives the candidate's recorded version, reads the final project files,
    /// and returns that project together with its effective configuration. The returned merge
    /// keeps the selected bootstrap alongside its original inputs.
    ///
    /// # Errors
    /// Returns a loading error or an error when no bootstrap graph matches the final workspace.
    pub fn workspace_for_bootstrap<'a, T, E: From<LockfileError>>(
        self,
        manifest: Option<&Value>,
        inputs: [MergeInput<'a>; 3],
        load: impl FnMut(Option<Version>) -> Result<(T, WorkspaceConfig), E>,
    ) -> Result<(T, WorkspaceConfig, ContextualMerge<'a>), E> {
        let bootstrap = bootstrap_locks(inputs);
        let (project, config, bootstrap) = select_workspace(
            self,
            manifest,
            bootstrap.each_ref().map(|lock| lock.as_deref()),
            load,
        )?;
        Ok((
            project,
            config,
            ContextualMerge {
                inputs,
                bootstrap: Some(bootstrap),
            },
        ))
    }
}

fn bootstrap_locks<'a>(inputs: [MergeInput<'a>; 3]) -> [Option<Cow<'a, Lockfile>>; 3] {
    inputs.map(|input| match input {
        MergeInput::Parsed(lock) => Some(Cow::Borrowed(lock)),
        MergeInput::Yaml(yaml) => RepairInput::bootstrap_lockfile(yaml).map(Cow::Owned),
    })
}

fn select_workspace<T, E: From<LockfileError>>(
    target: MergeTarget,
    manifest: Option<&Value>,
    locks: [Option<&Lockfile>; 3],
    mut load: impl FnMut(Option<Version>) -> Result<(T, WorkspaceConfig), E>,
) -> Result<(T, WorkspaceConfig, usize), E> {
    let versions = locks.map(|lock| lock.and_then(Lockfile::recorded_pnpm_version));
    if versions[0] == versions[1] && versions[0] == versions[2] {
        let (project, config) = load(versions[0].clone())?;
        let side = environment_source(locks, target, manifest, [Some(&config); 3])?;
        return Ok((project, config, side));
    }
    let mut candidates = versions.map(load);
    // Older runners can reject settings that are valid for the selected newer runner.
    if let [Err(first), Err(_), Err(_)] = candidates {
        return Err(first);
    }
    let configs = candidates
        .each_ref()
        .map(|result| result.as_ref().ok().map(|(_, config)| config));
    let side = environment_source(locks, target, manifest, configs)?;
    candidates.swap(0, side);
    let [selected, _, _] = candidates;
    let (project, config) = selected?;
    Ok((project, config, side))
}

// Package-manager dependencies and their transitive platform binaries form a
// separate lock document. Reuse the complete graph for the declared manager.
pub(super) fn environment_source(
    locks: [Option<&Lockfile>; 3],
    target: MergeTarget,
    root: Option<&Value>,
    configurations: [Option<&WorkspaceConfig>; 3],
) -> Result<usize, LockfileError> {
    let mut best = None;
    let mut matched_config = false;
    let mut wanted = None;
    for side in [target.index(), target.other_index(), Source::Base as usize] {
        let Some(config) = configurations[side] else {
            continue;
        };
        let lock = locks[side];
        if !config.matches_config_dependencies(lock.and_then(Lockfile::environment)) {
            continue;
        }
        matched_config = true;
        let recorded = lock.and_then(Lockfile::recorded_pnpm_version);
        let same_runner = config
            .pnpm_version
            .as_ref()
            .zip(recorded.as_ref())
            .is_none_or(|(v, pin)| v == pin);
        let policy = ManagerPolicy::for_workspace(root, Some(config), recorded.as_ref());
        let missing_pin = match policy {
            ManagerPolicy::Ignored => false,
            ManagerPolicy::Unspecified if same_runner => false,
            ManagerPolicy::Unspecified => continue,
            ManagerPolicy::Declared(manager) => {
                wanted = Some(manager.specifier);
                match lock.and_then(|lock| lock.matches_pnpm_manager(&manager)) {
                    Some(true) if same_runner || !manager.requires_pin => false,
                    None if !manager.requires_pin => true,
                    _ => continue,
                }
            }
        };
        let priority = (missing_pin, !same_runner);
        if priority == (false, false) {
            return Ok(side);
        }
        if best.is_none_or(|(old, _)| priority < old) {
            best = Some((priority, side));
        }
    }
    if let Some((_, side)) = best {
        return Ok(side);
    }
    if matched_config {
        let message = wanted.map_or_else(
            || "no bootstrap graph matches the configured pnpm version".to_owned(),
            |version| {
                format!(
                    "no locked bootstrap graph for pnpm@{version} matches the final configuration"
                )
            },
        );
        return Err(missing(
            &["importers", ".", "packageManagerDependencies", "pnpm"],
            message,
        ));
    }
    Err(missing(
        &["importers", ".", "configDependencies"],
        "no bootstrap graph matches the final configuration dependencies",
    ))
}
