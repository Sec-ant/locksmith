use crate::{
    Result, git,
    policy::Policy,
    registry,
    workspace::{self, Project},
};
use locksmith::{Lockfile, LockfileError, RepairContext, RepairInput};

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
    let project = Project::read(&mut blobs, commit, prefix)?;
    let policy = Policy::from_manifest(&project.root);
    let config = policy.configuration(&project.config)?;
    let patches = workspace::patches(&config, &mut blobs, commit, prefix)?;
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
    let input = RepairInput::parse(&text)?;
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
                    let previous = Project::read(&mut blobs, parent, prefix)?;
                    evidence.push((
                        lock,
                        Policy::from_manifest(&previous.root).configuration(&previous.config)?,
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
                    input.apply(&RepairContext {
                        parents: &parents,
                        registry,
                        ..context
                    })
                },
            )?
        }
        other => other?,
    };
    policy.check_peers(&project.config, &result, &manifests)?;
    Ok(result)
}
