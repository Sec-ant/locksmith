//! Converge the installed candidate in the current workspace against a Git ref.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
};

use locksmith::{Lockfile, ManifestChange};
use serde_json::Value;

use crate::{
    Result,
    git::{self, Blobs},
    policy::Policy,
    workspace::{self, Members, Project},
};

fn working_file(path: &str) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}").into()),
    }
}

pub fn run(reference: &str, allow_network: bool) -> Result<Lockfile> {
    let revision = git::output(&[
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{reference}^{{commit}}"),
    ])?;
    let tree = revision.trim_end();
    let prefix = git::output(&["rev-parse", "--show-prefix"])?;
    let prefix = prefix.trim_end_matches('\n');
    let mut blobs = Blobs::open()?;
    let project = Project::read(&mut blobs, tree, prefix)?;
    let working = Project::from_files(None, working_file)?;
    let policy = Policy::from_manifest(&working.root);
    let base_config = Policy::from_manifest(&project.root).configuration(&project.config)?;
    let config = policy.configuration(&working.config)?;
    let upgrade = base_config.changes_pnpm_major(&config);
    for key in project
        .config
        .as_object()
        .unwrap()
        .keys()
        .chain(working.config.as_object().unwrap().keys())
    {
        let a = project.config.get(key);
        let b = working.config.get(key);
        let supported = match key.as_str() {
            "catalog" | "catalogs" | "overrides" | "packageExtensions" => true,
            "patchedDependencies"
            | "onlyBuiltDependencies"
            | "allowBuilds"
            | "ignoredBuiltDependencies"
            | "trustLockfile"
            | "verifyDepsBeforeRun"
            | "pmOnFail"
            | "minimumReleaseAgeExclude" => upgrade,
            _ => false,
        };
        if a != b && !supported {
            return Err(format!("changed workspace setting {key} in pnpm-workspace.yaml or package.json requires resolution").into());
        }
    }
    if project.workspace != working.workspace {
        return Err("workspace membership changed in pnpm-workspace.yaml".into());
    }
    let mut files = if upgrade {
        workspace::hook_paths(&project.config)?
    } else {
        workspace::file_inputs(&project.config)?
    };
    files.push(".npmrc".into());
    for path in files {
        if blobs.read(tree, &format!("{prefix}{path}"))? != working_file(&path)? {
            return Err(
                format!("converge requires unchanged workspace configuration: {path}").into(),
            );
        }
    }
    let patches = if upgrade {
        workspace::patches(&config, "", working_file)?
    } else {
        BTreeMap::new()
    };
    let path = format!("{prefix}pnpm-lock.yaml");
    let baseline_text = blobs
        .read(tree, &path)?
        .ok_or_else(|| format!("missing {reference}:{path}"))?;
    let working_text = fs::read_to_string("pnpm-lock.yaml")?;
    let candidate_text = (baseline_text != working_text).then_some(working_text);

    let (baseline, candidate, [before, after]) = std::thread::scope(|scope| -> Result<_> {
        let parallel = candidate_text
            .as_ref()
            .is_some_and(|text| crate::parallel_parse(text.len() as u64));
        let baseline = scope.spawn(move || Lockfile::try_from(baseline_text));
        let manifests = || workspace_manifests(&project, &working, blobs, tree, prefix);
        let candidate = || candidate_text.map(Lockfile::try_from).transpose();
        let (manifests, candidate) = if parallel {
            let worker = scope.spawn(candidate);
            (
                manifests()?,
                worker.join().expect("lockfile parser panicked"),
            )
        } else {
            (manifests()?, candidate())
        };
        let baseline = baseline.join().expect("lockfile parser panicked")?;
        let candidate = candidate?.unwrap_or_else(|| baseline.clone());
        Ok((baseline, candidate, manifests))
    })?;
    let importers: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let changes: Vec<_> = importers
        .into_iter()
        .map(|id| ManifestChange {
            importer: id,
            before: before.get(id),
            after: after.get(id),
        })
        .collect();
    let result = crate::registry::resolve(
        crate::registry::Lookup::Converge,
        allow_network.then_some(crate::registry::ProjectRc::Working),
        &working.config,
        &after["."],
        |registry| {
            locksmith::converge_with_patches(
                &baseline,
                &candidate,
                &changes,
                locksmith::ConvergeOptions {
                    baseline_configuration: Some(&base_config),
                    configuration: Some(&config),
                    registry,
                },
                &patches,
            )
        },
    )?;
    policy.check_peers(&working.config, &result, &after)?;
    Ok(result)
}

fn workspace_manifests(
    project: &Project,
    working: &Project,
    mut blobs: Blobs,
    tree: &str,
    prefix: &str,
) -> Result<[BTreeMap<String, Value>; 2]> {
    let before = workspace::manifests(project, &mut blobs, tree, prefix)?;
    let mut after = BTreeMap::from([(".".into(), working.root.clone())]);
    if working.workspace {
        for id in Members::new(working)?.working_directories(prefix)? {
            if let Some(manifest) = workspace::read_manifest(&id, working_file)? {
                after.insert(id, manifest);
            }
        }
    }
    Ok([before, after])
}
