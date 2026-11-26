use std::{collections::BTreeMap, error::Error, fs, path::Path, thread};

use locksmith::{Lockfile, LockfileError, MergeContext, MergeInput, MergeTarget};
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

pub(crate) enum Sources {
    Parsed([Lockfile; 3]),
    Yaml([String; 3]),
}

impl Sources {
    pub fn read<P: AsRef<Path>>(files: [&P; 3]) -> Result<Self> {
        match read_three(files) {
            Ok(locks) => Ok(Self::Parsed(locks)),
            Err(e)
                if matches!(
                    e.downcast_ref::<LockfileError>(),
                    Some(LockfileError::Parse(_))
                ) =>
            {
                let [base, ours, theirs] = files.map(fs::read_to_string);
                Ok(Self::Yaml([base?, ours?, theirs?]))
            }
            Err(e) => Err(e as Box<dyn Error>),
        }
    }

    pub(crate) fn inputs(&self) -> [MergeInput<'_>; 3] {
        match self {
            Self::Parsed(locks) => locks.each_ref().map(MergeInput::from),
            Self::Yaml(text) => text.each_ref().map(|s| MergeInput::from(s.as_str())),
        }
    }

    pub fn with_context(
        &self,
        context: &MergeContext<'_>,
        raw: &Value,
        network: Option<ProjectRc<'_>>,
    ) -> Result<Lockfile> {
        let result = crate::registry::resolve(
            crate::registry::Lookup::Merge,
            network,
            raw,
            context
                .configuration
                .pnpm_version
                .as_ref()
                .map(|v| v.major()),
            |registry| {
                let context = MergeContext {
                    registry,
                    ..*context
                };
                if let Self::Parsed(locks) = self
                    && registry.is_some()
                {
                    return context.merge_with_requests(locks.each_ref());
                }
                let [base, ours, theirs] = self.inputs();
                locksmith::merge(base, ours, theirs, Some(&context)).map_err(|e| vec![e])
            },
        )?;
        Policy::from_configuration(context.configuration).check_peers(
            raw,
            &result,
            context.manifests,
        )?;
        Ok(result)
    }
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
    let root = manifests.get(".").ok_or("missing root manifest")?;
    let source: [Value; 3] = serde_json::from_value(raw["configurations"].take())?;
    let inputs = Sources::read(files)?;
    let (configurations, config) = target.workspace_configurations(
        Some(root),
        inputs.inputs().each_ref(),
        source.each_ref(),
        &raw["configuration"],
    )?;
    let patches = serde_json::from_value(raw["patches"].take())?;
    let context = MergeContext {
        registry: None,
        target,
        manifests: &manifests,
        configurations: configurations.each_ref(),
        configuration: &config,
        patches: &patches,
    };
    inputs.with_context(
        &context,
        &raw["configuration"],
        allow_network.then_some(ProjectRc::Working),
    )
}
