//! Select a complete pnpm bootstrap graph and interpret the project with its runner.
use super::{MergeTarget, Source};
use crate::error::missing;
use crate::{Lockfile, LockfileError, Value, WorkspaceConfig, manifest::ManagerPolicy};

// Package-manager dependencies and their transitive platform binaries form a
// separate lock document. Reuse the complete graph for the declared manager.
pub(crate) fn environment_source<'a>(
    locks: [Option<&Lockfile>; 3],
    target: MergeTarget,
    root: Option<&Value>,
    mut configuration: impl FnMut(usize) -> Option<&'a WorkspaceConfig>,
    duplicate_snapshots: [&[String]; 3],
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
        if let Some(env) = lock.and_then(Lockfile::environment) {
            // Recovery checks each duplicate snapshot body after selecting an environment.
            let issues = crate::graph::validate_with_unresolved_snapshots(
                env,
                crate::DocumentKind::Environment,
                Some(config),
                duplicate_snapshots[side],
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
