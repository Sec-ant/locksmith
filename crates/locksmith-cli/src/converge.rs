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
    #[cfg(feature = "bench-profile")]
    let revision_span = locksmith::profile::span("git_revision");
    let (tree, prefix) = git::revision_and_prefix(reference)?;
    let (tree, prefix) = (tree.as_str(), prefix.as_str());
    #[cfg(feature = "bench-profile")]
    drop(revision_span);
    let mut blobs = Blobs::open()?;
    let project = Project::read(&mut blobs, tree, prefix, None)?;
    let working = Project::from_files(None, working_file)?;
    let policy = Policy::from_project(&working);
    let path = format!("{prefix}pnpm-lock.yaml");
    let baseline_text = blobs
        .read(tree, &path)?
        .ok_or_else(|| format!("missing {reference}:{path}"))?;
    let working_text = fs::read_to_string("pnpm-lock.yaml")?;
    let base_config = project.configuration()?;
    let config = working.bootstrap_configuration(Some(working_text.as_str().into()), || {
        working_file(".npmrc")
    })?;
    let upgrade = base_config.changes_pnpm_version(&config);
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
            "catalog" | "catalogs" | "overrides" | "packageExtensions" | "configDependencies" => {
                true
            }
            "patchedDependencies"
            | "onlyBuiltDependencies"
            | "allowBuilds"
            | "ignoredBuiltDependencies"
            | "trustLockfile"
            | "verifyDepsBeforeRun"
            | "pmOnFail"
            | "ignorePnpmfile"
            | "minimumReleaseAgeExclude"
            | "pnpmfileChecksum" => upgrade,
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
        let mut hooks = workspace::hook_paths(&project.config)?;
        hooks.extend(workspace::hook_paths(&working.config)?);
        hooks.sort_unstable();
        hooks.dedup();
        hooks
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
    let candidate_text = (baseline_text != working_text).then_some(working_text);

    #[cfg(feature = "bench-profile")]
    let inputs_span = locksmith::profile::span("converge_input_collection");
    let (baseline, candidate, [mut before, mut after]) =
        std::thread::scope(|scope| -> Result<_> {
            // The working lock approximates the combined work of both parsers.
            let parallel = candidate_text
                .as_ref()
                .is_some_and(|text| crate::parallel_parse((text.len() as u64).saturating_mul(2)));
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
    #[cfg(feature = "bench-profile")]
    drop(inputs_span);
    let patches = if base_config.requires_bootstrap_reconciliation(&config, &baseline, &candidate) {
        workspace::patches(&config, "", working_file)?
    } else {
        BTreeMap::new()
    };
    let recorded =
        include_generated_members(&working, &baseline, &candidate, &mut before, &mut after)?;
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
        working.pnpm_version.as_ref().map(|v| v.major()),
        |registry| {
            locksmith::converge_with_recorded_baselines(
                &baseline,
                &candidate,
                &changes,
                locksmith::ConvergeOptions {
                    baseline_configuration: Some(&base_config),
                    configuration: Some(&config),
                    registry,
                },
                &patches,
                &recorded,
            )
            .map_err(|e| vec![e])
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
    #[cfg(feature = "bench-profile")]
    let _span = locksmith::profile::span("converge_manifest_collection");
    let mut after = BTreeMap::from([(".".into(), working.root.clone())]);
    if !working.workspace {
        return Ok([
            workspace::manifests(project, &mut blobs, tree, prefix)?,
            after,
        ]);
    }
    let (before, after) = std::thread::scope(|scope| {
        let before = scope.spawn(move || {
            workspace::manifests(project, &mut blobs, tree, prefix)
                .map_err(|error| error.to_string())
        });
        let after = (|| -> Result<_> {
            #[cfg(feature = "bench-profile")]
            let _span = locksmith::profile::span("worktree_manifests");
            for id in Members::new(working)?.working_directories(prefix)? {
                if let Some(manifest) = workspace::read_manifest(&id, working_file)? {
                    after.insert(id, manifest);
                }
            }
            Ok(after)
        })();
        (
            before.join().expect("committed manifest reader panicked"),
            after,
        )
    });
    Ok([before?, after?])
}

/// Installed importers can name generated projects whose manifests Git ignores.
fn include_generated_members(
    project: &Project,
    baseline: &Lockfile,
    candidate: &Lockfile,
    before: &mut BTreeMap<String, Value>,
    after: &mut BTreeMap<String, Value>,
) -> Result<BTreeSet<String>> {
    let mut recorded = BTreeSet::new();
    if !project.workspace {
        return Ok(recorded);
    }
    let [base, installed] =
        [baseline, candidate].map(|lock| lock.project().and_then(|d| d["importers"].as_object()));
    let unknown: BTreeSet<_> = base
        .into_iter()
        .flatten()
        .filter(|(id, _)| !before.contains_key(*id))
        .chain(
            installed
                .into_iter()
                .flatten()
                .filter(|(id, _)| !after.contains_key(*id)),
        )
        .map(|(id, _)| id)
        .collect();
    if unknown.is_empty() {
        return Ok(recorded);
    }
    let directories =
        Members::directories_on_disk(project, &std::env::current_dir()?.canonicalize()?)?;
    let patterns = Members::new(project)?;
    for id in unknown {
        if !patterns.contains(id) {
            continue;
        }
        if !after.contains_key(id)
            && directories.contains_key(id)
            && let Some(manifest) = workspace::read_manifest(id, working_file)?
        {
            after.insert(id.clone(), manifest);
        }
        if !before.contains_key(id)
            && let Some(record) = base.and_then(|base| base.get(id))
        {
            let old = locked_manifest(
                id,
                record,
                installed.and_then(|map| map.get(id)),
                after.get(id),
            )?;
            before.insert(id.clone(), old);
            recorded.insert(id.clone());
        }
    }
    Ok(recorded)
}

fn locked_manifest(
    id: &str,
    record: &Value,
    installed: Option<&Value>,
    current: Option<&Value>,
) -> Result<Value> {
    // An unchanged importer can supply the current peer and package context.
    // Its locked specifiers can still differ from the authored declarations.
    let groups = ["dependencies", "devDependencies", "optionalDependencies"];
    let same_importer = installed == Some(record);
    let mut manifest = serde_json::json!({});
    if let Some(current) = current {
        for (field, value) in current.as_object().expect("parsed workspace manifest") {
            if matches!(field.as_str(), "devEngines" | "pnpm")
                || same_importer && !groups.contains(&field.as_str())
            {
                manifest[field] = value.clone();
            }
        }
        // pnpm records an auto-installed peer in dependencies. Its pin may
        // drift even when both lockfiles record the same effective range.
        if !same_importer && let Some(peers) = current["peerDependencies"].as_object() {
            for (name, specifier) in peers {
                let before = record["dependencies"]
                    .get(name)
                    .and_then(|dep| dep.get("specifier"));
                let after = installed.and_then(|r| r["dependencies"].get(name)?.get("specifier"));
                if before.is_some()
                    && before == after
                    && groups.iter().all(|g| current[g].get(name).is_none())
                {
                    manifest["peerDependencies"][name] = specifier.clone();
                }
            }
        }
    }
    if let Some(meta) = record.get("dependenciesMeta") {
        manifest["dependenciesMeta"] = meta.clone();
    }
    if let Some(directory) = record.get("publishDirectory") {
        manifest["publishConfig"]["directory"] = directory.clone();
        if let Some(link) = record.get("linkDirectory") {
            manifest["publishConfig"]["linkDirectory"] = link.clone();
        }
    }
    for group in groups {
        if let Some(entries) = record.get(group) {
            for (name, edge) in entries
                .as_object()
                .ok_or_else(|| format!("invalid baseline importer {id}/{group}"))?
            {
                let spec = edge["specifier"]
                    .as_str()
                    .ok_or_else(|| format!("missing baseline specifier for {id}/{group}/{name}"))?;
                manifest[group][name] = Value::from(spec);
            }
        }
    }
    Ok(manifest)
}
