use crate::{
    Result, git,
    policy::Policy,
    registry,
    workspace::{self, Project},
};
use locksmith::{Lockfile, LockfileError, MergeContext, MergeTarget, RepairContext, RepairInput};

pub fn run(reference: &str, allow_network: bool) -> Result<Lockfile> {
    let revision = git::output(&[
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{reference}^{{commit}}"),
    ])?;
    let commit = revision.trim_end();
    let prefix = git::output(&["rev-parse", "--show-prefix"])?;
    let prefix = prefix.trim_end_matches('\n');
    let file = format!("{prefix}pnpm-lock.yaml");
    let mut blobs = git::Blobs::open()?;
    let text = blobs
        .read(commit, &file)?
        .ok_or_else(|| format!("missing {reference}:{file}"))?;
    let version = RepairInput::recorded_pnpm_version(&text);
    let project = Project::read(&mut blobs, commit, prefix, version)?;
    let policy = Policy::from_project(&project);
    let config = policy.configuration(&project.config)?;
    let patches = workspace::patches(&config, prefix, |file| blobs.read(commit, file))?;
    let manifests = workspace::manifests(&project, &mut blobs, commit, prefix)?;
    // Release Git's mapped packfiles before allocating the lockfile graph.
    drop(blobs);
    let context = RepairContext {
        manifests: &manifests,
        configuration: &config,
        patches: &patches,
        parents: &[],
        registry: None,
    };
    let input = match RepairInput::parse(&text) {
        Err(error)
            if matches!(&error, LockfileError::ResolutionRequired { path, .. }
                if matches!(path.as_slice(), [section, _] if section == "snapshots")) =>
        {
            recover_merge_snapshots(&text, commit, prefix, &context)?.ok_or(error)?
        }
        other => other?,
    };
    let result = match input.apply(&context) {
        Err(LockfileError::ResolutionRequired { .. }) => {
            let revisions = git::output(&["rev-list", "--parents", "-1", commit])?;
            let mut blobs = git::Blobs::open()?;
            let mut evidence = Vec::new();
            for parent in revisions.split_whitespace().skip(1) {
                if let Some(source) = blobs.read(parent, &file)?
                    && source != text
                {
                    let lock = match Lockfile::try_from(source) {
                        Ok(lock) => lock,
                        Err(e) => {
                            eprintln!("locksmith: skipping unusable parent {parent}: {e}");
                            continue;
                        }
                    };
                    let previous =
                        Project::read(&mut blobs, parent, prefix, lock.recorded_pnpm_version())?;
                    evidence.push((
                        lock,
                        Policy::from_project(&previous).configuration(&previous.config)?,
                    ));
                }
            }
            let parents: Vec<_> = evidence.iter().map(|(l, c)| (l, c)).collect();
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
                &project.root,
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
        other => other?,
    };
    policy.check_peers(&project.config, &result, &manifests)?;
    Ok(result)
}

fn recover_merge_snapshots(
    text: &str,
    commit: &str,
    prefix: &str,
    context: &RepairContext<'_>,
) -> Result<Option<RepairInput>> {
    let history = git::output(&["rev-list", "--parents", "-1", commit])?;
    let parents: Vec<_> = history.split_whitespace().skip(1).collect();
    let [ours, theirs] = parents.as_slice() else {
        return Ok(None);
    };
    let base = git::merge_base(ours, theirs)?;
    let mut blobs = git::Blobs::open()?;
    let file = format!("{prefix}pnpm-lock.yaml");
    let [base, ours, theirs] = [base.as_str(), ours, theirs].map(|revision| -> Result<_> {
        let text = blobs
            .read(revision, &file)?
            .ok_or_else(|| format!("missing {revision}:{file}"))?;
        let lock = Lockfile::try_from(text)?;
        let project = Project::read(&mut blobs, revision, prefix, lock.recorded_pnpm_version())?;
        let config = Policy::from_project(&project).configuration(&project.config)?;
        Ok((lock, config))
    });
    let ((base, base_cfg), (ours, ours_cfg), (theirs, theirs_cfg)) = (base?, ours?, theirs?);
    drop(blobs);
    // The first parent is the committed merge's target, independent of current refs.
    let witness = locksmith::merge(
        &base,
        &ours,
        &theirs,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: context.manifests,
            configurations: [&base_cfg, &ours_cfg, &theirs_cfg],
            configuration: context.configuration,
            patches: context.patches,
            registry: None,
        }),
    )?;
    Ok(Some(RepairInput::parse_with_snapshot_reference(
        text, &witness,
    )?))
}
