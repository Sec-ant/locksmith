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
    workspace::{Members, Project, file_inputs},
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
    let working = Project::from_files(working_file)?;
    for key in project
        .config
        .as_object()
        .unwrap()
        .keys()
        .chain(working.config.as_object().unwrap().keys())
    {
        let a = project.config.get(key);
        let b = working.config.get(key);
        if !matches!(
            key.as_str(),
            "catalog" | "catalogs" | "overrides" | "packageExtensions"
        ) && a != b
        {
            return Err(format!("changed workspace setting {key} in pnpm-workspace.yaml or package.json requires resolution").into());
        }
    }
    if project.workspace != working.workspace {
        return Err("workspace membership changed in pnpm-workspace.yaml".into());
    }
    let mut files: BTreeSet<_> = file_inputs(&project.config)?.into_iter().collect();
    files.insert(".npmrc".into());
    for path in files {
        if blobs.read(tree, &format!("{prefix}{path}"))? != working_file(&path)? {
            return Err(
                format!("converge requires unchanged workspace configuration: {path}").into(),
            );
        }
    }
    let members = Members::new(&project)?;
    let mut importers = BTreeSet::from([".".to_owned()]);
    if project.workspace {
        importers.extend(members.directories(&git::workspace_paths(tree, prefix)?, prefix)?);
        let paths = git::output(&[
            "ls-files",
            "-zc",
            "--others",
            "--exclude-standard",
            "--",
            ":(glob)**/package.json",
            ":(glob)**/package.json5",
            ":(glob)**/package.yaml",
        ])?;
        importers.extend(members.directories(&paths, "")?);
    }
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    for id in &importers {
        let path = if id == "." {
            "package.json".into()
        } else {
            format!("{id}/package.json")
        };
        for (text, manifests) in [
            (blobs.read(tree, &format!("{prefix}{path}"))?, &mut before),
            (working_file(&path)?, &mut after),
        ] {
            if let Some(text) = text {
                let manifest: Value =
                    serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
                manifests.insert(id.clone(), manifest);
            }
        }
    }
    let changes: Vec<_> = importers
        .iter()
        .filter(|id| before.contains_key(*id) || after.contains_key(*id))
        .map(|id| ManifestChange {
            importer: id,
            before: before.get(id),
            after: after.get(id),
        })
        .collect();
    let path = format!("{prefix}pnpm-lock.yaml");
    let baseline_text = blobs
        .read(tree, &path)?
        .ok_or_else(|| format!("missing {reference}:{path}"))?;
    drop(blobs);
    let candidate_text = fs::read_to_string("pnpm-lock.yaml")?;
    let candidate_text = (baseline_text != candidate_text).then_some(candidate_text);
    let baseline = Lockfile::try_from(baseline_text)?;
    let candidate = match candidate_text {
        Some(text) => Lockfile::try_from(text)?,
        None => baseline.clone(),
    };
    let policy = Policy::from_manifest(after.get(".").ok_or("missing working tree package.json")?);
    let base_config = Policy::from_manifest(&project.root).configuration(&project.config)?;
    let config = policy.configuration(&working.config)?;
    let result = crate::registry::resolve(
        crate::registry::Lookup::Converge,
        allow_network.then_some(crate::registry::ProjectRc::Working),
        &working.config,
        &after["."],
        |registry| {
            locksmith::converge(
                &baseline,
                &candidate,
                &changes,
                locksmith::ConvergeOptions {
                    baseline_configuration: Some(&base_config),
                    configuration: Some(&config),
                    registry,
                },
            )
        },
    )?;
    policy.check_peers(&working.config, &result, &after)?;
    Ok(result)
}
