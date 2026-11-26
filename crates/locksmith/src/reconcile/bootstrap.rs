//! Select a complete pnpm bootstrap graph and interpret the project with its runner.
use std::{cell::OnceCell, fmt::Display};

use node_semver::Version;

use super::{MergeTarget, Source, missing};
use crate::{
    ContextualMerge, Lockfile, LockfileError, MergeInput, Value, WorkspaceConfig,
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
        let bootstrap = inputs.map(MergeInput::bootstrap);
        let locks = bootstrap.each_ref().map(|(lock, _)| lock.as_deref());
        let [base, ours, theirs] = [0, 1, 2].map(|i| {
            let version = locks[i].and_then(Lockfile::recorded_pnpm_version);
            WorkspaceConfig::from_pnpm_source(branches[i], manifest, version)
        });
        let ((), config, bootstrap) = select_workspace(
            self,
            manifest,
            locks,
            bootstrap.each_ref().map(|(_, provisional)| *provisional),
            |lock| {
                let version = lock.and_then(Lockfile::recorded_pnpm_version);
                WorkspaceConfig::from_pnpm_context(final_settings, manifest, version)
                    .map(|config| ((), config))
            },
        )
        .map_err(|error| input_error(inputs, error))?;
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
    pub fn workspace_for_bootstrap<'a, T, E: From<LockfileError> + Display>(
        self,
        manifest: Option<&Value>,
        inputs: [MergeInput<'a>; 3],
        mut load: impl FnMut(Option<Version>) -> Result<(T, WorkspaceConfig), E>,
    ) -> Result<(T, WorkspaceConfig, ContextualMerge<'a>), E> {
        self.workspace_for_candidate(manifest, inputs, |lock| {
            load(lock.and_then(Lockfile::recorded_pnpm_version))
        })
    }

    /// Select a bootstrap graph using the settings needed to interpret each candidate.
    /// The loader can use explicit artifact URLs in that lock without resolving registry routes.
    ///
    /// # Errors
    /// Returns a loading error or an error when no bootstrap graph matches the final workspace.
    pub fn workspace_for_candidate<'a, T, E: From<LockfileError> + Display>(
        self,
        manifest: Option<&Value>,
        inputs: [MergeInput<'a>; 3],
        load: impl FnMut(Option<&Lockfile>) -> Result<(T, WorkspaceConfig), E>,
    ) -> Result<(T, WorkspaceConfig, ContextualMerge<'a>), E> {
        let bootstrap = inputs.map(MergeInput::bootstrap);
        let (project, config, bootstrap) = select_workspace(
            self,
            manifest,
            bootstrap.each_ref().map(|(lock, _)| lock.as_deref()),
            bootstrap.each_ref().map(|(_, provisional)| *provisional),
            load,
        )
        .map_err(|error| input_error(inputs, error))?;
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

fn input_error<E: From<LockfileError>>(inputs: [MergeInput<'_>; 3], error: E) -> E {
    inputs
        .into_iter()
        .find_map(|input| match input {
            MergeInput::Yaml(text) => crate::recovery::candidate(text).err().map(E::from),
            MergeInput::Parsed(_) => None,
        })
        .unwrap_or(error)
}

fn select_workspace<T, E: From<LockfileError> + Display>(
    target: MergeTarget,
    manifest: Option<&Value>,
    locks: [Option<&Lockfile>; 3],
    provisional: [bool; 3],
    mut load: impl FnMut(Option<&Lockfile>) -> Result<(T, WorkspaceConfig), E>,
) -> Result<(T, WorkspaceConfig, usize), E> {
    let mut candidates = [OnceCell::new(), OnceCell::new(), OnceCell::new()];
    let selected = environment_source(
        locks,
        target,
        manifest,
        |side| {
            candidates[side]
                .get_or_init(|| load(locks[side]))
                .as_ref()
                .ok()
                .map(|(_, config)| config)
        },
        provisional,
    );
    let side = match selected {
        Ok(side) => side,
        Err(mut error) => {
            // An older runner can reject settings valid for a newer candidate.
            let candidates = candidates.map(OnceCell::into_inner);
            if let [Some(Err(first)), Some(Err(_)), Some(Err(_))] = candidates {
                return Err(first);
            }
            let failures: String = ["base", "ours", "theirs"]
                .into_iter()
                .zip(candidates)
                .filter_map(|(side, candidate)| {
                    candidate?
                        .err()
                        .map(|load| format!("\n{side} workspace: {load}"))
                })
                .collect();
            if !failures.is_empty() {
                if let LockfileError::ResolutionRequired { message, .. } = &mut error {
                    message.push_str(&failures);
                } else {
                    error = LockfileError::WorkspaceInput(format!("{error}{failures}"));
                }
            }
            return Err(error.into());
        }
    };
    let (project, config) = candidates[side]
        .take()
        .expect("selected candidate was loaded")?;
    Ok((project, config, side))
}

// Package-manager dependencies and their transitive platform binaries form a
// separate lock document. Reuse the complete graph for the declared manager.
pub(super) fn environment_source<'a>(
    locks: [Option<&Lockfile>; 3],
    target: MergeTarget,
    root: Option<&Value>,
    mut configuration: impl FnMut(usize) -> Option<&'a WorkspaceConfig>,
    provisional: [bool; 3],
) -> Result<usize, LockfileError> {
    let mut best = None;
    let mut invalid_environment = None;
    let mut matched_config = false;
    let mut wanted = None;
    for side in [target.index(), target.other_index(), Source::Base as usize] {
        let Some(config) = configuration(side) else {
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
        // Recovery validates each interpretation of duplicated snapshots after selection.
        if !provisional[side]
            && let Some(env) = lock.and_then(Lockfile::environment)
        {
            let issues = crate::graph::validate_with_config(
                env,
                crate::DocumentKind::Environment,
                Some(config),
            );
            if !issues.is_empty() {
                invalid_environment.get_or_insert(issues);
                continue;
            }
        }
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
    if let Some(issues) = invalid_environment {
        return Err(LockfileError::InvalidGraph(issues));
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
