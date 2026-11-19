use std::{collections::BTreeMap, error::Error, fs, path::Path, thread};

use locksmith::{Lockfile, LockfileError, MergeContext, MergeTarget};
use serde_json::Value;

use crate::registry::ProjectRc;
use crate::{Result, policy::Policy};

type ReadResult<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn read_source(path: &Path) -> ReadResult<Lockfile> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Lockfile::try_from(text)?)
}

pub(crate) fn read_three<P: AsRef<Path>>(files: [&P; 3]) -> ReadResult<[Lockfile; 3]> {
    let paths = files.map(AsRef::as_ref);
    if paths[0]
        .metadata()
        .is_ok_and(|file| crate::parallel_parse(file.len()))
    {
        return thread::scope(|scope| {
            let spawn =
                |path| thread::Builder::new().spawn_scoped(scope, move || read_source(path));
            let base = spawn(paths[0]);
            let ours = spawn(paths[1]);
            let theirs = read_source(paths[2]);
            let finish =
                |worker: std::io::Result<thread::ScopedJoinHandle<'_, ReadResult<Lockfile>>>,
                 path| {
                    match worker {
                        Ok(worker) => worker.join().expect("lockfile parser"),
                        Err(_) => read_source(path),
                    }
                };
            Ok([finish(base, paths[0])?, finish(ours, paths[1])?, theirs?])
        });
    }
    Ok([
        read_source(paths[0])?,
        read_source(paths[1])?,
        read_source(paths[2])?,
    ])
}

pub fn with_context<P: AsRef<Path>>(
    files: [&P; 3],
    context: &MergeContext<'_>,
    policy: &Policy,
    raw: &Value,
    network: Option<ProjectRc<'_>>,
) -> Result<Lockfile> {
    let parsed = read_three(files);
    let parsed = match parsed {
        Ok(locks) => Ok(locks),
        Err(e)
            if matches!(
                e.downcast_ref::<LockfileError>(),
                Some(LockfileError::Parse(_))
            ) =>
        {
            let [base, ours, theirs] = files.map(fs::read_to_string);
            Err([base?, ours?, theirs?])
        }
        Err(e) => return Err(e as Box<dyn Error>),
    };
    let result = crate::registry::resolve(
        crate::registry::Lookup::Merge,
        network,
        raw,
        &context.manifests["."],
        |registry| {
            let context = MergeContext {
                registry,
                ..*context
            };
            match &parsed {
                Ok([b, o, t]) => locksmith::merge(b, o, t, Some(&context)),
                Err([b, o, t]) => {
                    locksmith::merge(b.as_str(), o.as_str(), t.as_str(), Some(&context))
                }
            }
        },
    )?;
    policy.check_peers(raw, &result, context.manifests)?;
    Ok(result)
}

pub fn from_file<P: AsRef<Path>>(
    files: [&P; 3],
    path: &P,
    allow_network: bool,
) -> Result<Lockfile> {
    let mut raw: Value = serde_json::from_slice(&fs::read(path)?)?;
    let target = match raw["target"].as_str() {
        Some("ours") => MergeTarget::Ours,
        Some("theirs") => MergeTarget::Theirs,
        _ => return Err("context.target must be ours or theirs".into()),
    };
    let manifests: BTreeMap<String, Value> = serde_json::from_value(raw["manifests"].take())?;
    let policy = Policy::from_manifest(manifests.get(".").ok_or("missing root manifest")?);
    let source: [Value; 3] = serde_json::from_value(raw["configurations"].take())?;
    let [base, ours, theirs] = source.each_ref().map(|s| policy.configuration(s));
    let configurations = [base?, ours?, theirs?];
    let config = policy.configuration(&raw["configuration"])?;
    let patches = serde_json::from_value(raw["patches"].take())?;
    let context = MergeContext {
        registry: None,
        target,
        manifests: &manifests,
        configurations: configurations.each_ref(),
        configuration: &config,
        patches: &patches,
    };
    with_context(
        files,
        &context,
        &policy,
        &raw["configuration"],
        allow_network.then_some(ProjectRc::Working),
    )
}
