use crate::{
    Result, git,
    policy::Policy,
    registry,
    ui::Ui,
    workspace::{self, Project},
};
use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeTarget, RepairContext, RepairInput, WorkspaceConfig,
};

pub fn run(reference: &str, allow_network: bool, ui: &Ui) -> Result<(String, Lockfile)> {
    #[cfg(feature = "bench-profile")]
    let revision_span = locksmith::profile::span("git_revision");
    let (commit, prefix) = git::revision_and_prefix(reference)?;
    let (commit, prefix) = (commit.as_str(), prefix.as_str());
    #[cfg(feature = "bench-profile")]
    drop(revision_span);
    let file = format!("{prefix}pnpm-lock.yaml");
    let mut blobs = git::Blobs::open(workspace::CONFIG_FILES, &workspace::DEFAULT_HOOK_FILES)?;
    let text = blobs
        .read(commit, &file)?
        .ok_or_else(|| format!("missing {reference}:{file}"))?;
    let version = RepairInput::recorded_pnpm_version(&text);
    let project = Project::read(&mut blobs, commit, prefix, version)?;
    let policy = Policy::from_project(&project);
    let config = project.bootstrap_configuration(Some(text.as_str().into()), || {
        blobs.read(commit, &format!("{prefix}.npmrc"))
    })?;
    let patches = workspace::patches(&config, prefix, |file| blobs.read(commit, file))?;
    let manifests = workspace::manifests(&project, &mut blobs, commit, prefix)?;
    let context = RepairContext {
        manifests: &manifests,
        configuration: &config,
        patches: &patches,
        parents: &[],
        registry: None,
    };
    let mut merge_parents = None;
    let input = match RepairInput::parse(&text) {
        Err(error)
            if matches!(&error, LockfileError::ResolutionRequired { path, .. }
                if matches!(path.as_slice(), [section, _] if section == "snapshots")) =>
        {
            let parents = parent_revisions(commit)?;
            let recovered = recover_merge_snapshots(&text, prefix, &context, &mut blobs, &parents)?
                .ok_or(error)?;
            merge_parents = Some(recovered.parents);
            recovered.input
        }
        other => other?,
    };
    let result = match input.apply(&context) {
        Err(LockfileError::ResolutionRequired { .. }) => {
            input.validate_bootstrap(&context)?;
            let evidence = match merge_parents {
                Some(parents) => parents.into_iter().flatten().collect(),
                None => {
                    let mut evidence = Vec::new();
                    for parent in parent_revisions(commit)? {
                        if let Some(source) = blobs.read(&parent, &file)?
                            && source != text
                        {
                            let parent_evidence = (|| -> Result<_> {
                                let lock = Lockfile::try_from(source)?;
                                let config =
                                    parent_configuration(&mut blobs, &parent, prefix, &lock)?;
                                Ok(ParentEvidence { lock, config })
                            })();
                            match parent_evidence {
                                Ok(parent) => evidence.push(parent),
                                Err(e) => {
                                    ui.warning(format!("skipping unusable parent {parent}: {e}"));
                                }
                            }
                        }
                    }
                    evidence
                }
            };
            let parents: Vec<_> = evidence.iter().map(|p| (&p.lock, &p.config)).collect();
            let rc = format!("{prefix}.npmrc");
            let network = if allow_network {
                Some((blobs.read(commit, &rc)?, blobs.read("HEAD", &rc)?))
            } else {
                None
            };
            drop(blobs);
            registry::resolve(
                registry::Lookup::Merge,
                network
                    .as_ref()
                    .map(|(settings, credentials)| registry::ProjectRc::Git {
                        settings: settings.as_deref(),
                        credentials: credentials.as_deref(),
                    }),
                &project.config,
                project.pnpm_version.as_ref().map(|v| v.major()),
                ui,
                |registry| {
                    input
                        .apply(&RepairContext {
                            parents: &parents,
                            registry,
                            ..context
                        })
                        .map_err(|e| vec![e])
                },
            )?
        }
        other => {
            drop(blobs);
            other?
        }
    };
    policy.check_peers(&project.config, &result, &manifests, ui)?;
    Ok((text, result))
}

struct ParentEvidence {
    lock: Lockfile,
    config: WorkspaceConfig,
}

struct RecoveredMerge {
    input: RepairInput,
    parents: [Option<ParentEvidence>; 2],
}

fn parent_revisions(commit: &str) -> Result<Vec<String>> {
    Ok(git::output(&["rev-list", "--parents", "-1", commit])?
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect())
}

fn parent_configuration(
    blobs: &mut git::Blobs,
    revision: &str,
    prefix: &str,
    lock: &Lockfile,
) -> Result<WorkspaceConfig> {
    Project::read(blobs, revision, prefix, lock.recorded_pnpm_version())?.configuration()
}

fn recover_merge_snapshots(
    text: &str,
    prefix: &str,
    context: &RepairContext<'_>,
    blobs: &mut git::Blobs,
    parents: &[String],
) -> Result<Option<RecoveredMerge>> {
    let [ours, theirs] = parents else {
        return Ok(None);
    };
    let base = git::merge_base(ours, theirs)?;
    let file = format!("{prefix}pnpm-lock.yaml");
    let [base, ours, theirs] = [base.as_str(), ours, theirs].map(|revision| -> Result<_> {
        let source = blobs
            .read(revision, &file)?
            .ok_or_else(|| format!("missing {revision}:{file}"))?;
        let identical = source == text;
        let lock = Lockfile::try_from(source)?;
        let config = parent_configuration(blobs, revision, prefix, &lock)?;
        Ok((ParentEvidence { lock, config }, identical))
    });
    let ((base, _), (ours, ours_identical), (theirs, theirs_identical)) = (base?, ours?, theirs?);
    // The first parent is the committed merge's target, independent of current refs.
    let input = RepairInput::parse_with_merge_witness(
        text,
        [&base.lock, &ours.lock, &theirs.lock],
        &MergeContext {
            target: MergeTarget::Ours,
            manifests: context.manifests,
            configurations: [&base.config, &ours.config, &theirs.config],
            configuration: context.configuration,
            patches: context.patches,
            registry: None,
        },
    )?;
    Ok(Some(RecoveredMerge {
        input,
        parents: [
            (!ours_identical).then_some(ours),
            (!theirs_identical).then_some(theirs),
        ],
    }))
}
