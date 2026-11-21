//! Node-API bindings for the in-memory engine. File and Git operations use the CLI.
mod context;
mod diagnostic;

use context::{Changes, Converge, Merge, PeerOptions, Repair, decode};
use locksmith::{Lockfile, LockfileError, Value};
use napi::{
    Env, Task,
    bindgen_prelude::{AsyncTask, Either, ToNapiValue, TypeName},
};
use napi_derive::napi;
use serde_json::json;
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, LockfileError>;
type Work<T> = Box<dyn FnOnce() -> Result<T> + Send>;

pub struct Job<T>(Option<Work<T>>);

impl<T: Send + ToNapiValue + TypeName + 'static> Task for Job<T> {
    type Output = Result<T>;
    type JsValue = T;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        Ok(self.0.take().expect("a Node task runs once")())
    }
    fn resolve(&mut self, env: Env, output: Result<T>) -> napi::Result<T> {
        match output {
            Ok(value) => Ok(value),
            Err(error) => Err(diagnostic::exception(&env, error)?),
        }
    }
}

fn job<T: Send + ToNapiValue + TypeName + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> AsyncTask<Job<T>> {
    AsyncTask::new(Job(Some(Box::new(work))))
}

#[napi(js_name = "Lockfile")]
pub struct ParsedLockfile(Lockfile);

#[napi]
impl ParsedLockfile {
    #[napi]
    pub fn to_yaml(&self) -> AsyncTask<Job<String>> {
        let lock = self.0.clone();
        job(move || lock.to_yaml())
    }

    #[napi]
    pub fn document(&self, kind: String) -> AsyncTask<Job<Option<BTreeMap<String, Value>>>> {
        let lock = self.0.clone();
        job(move || {
            let kind = match kind.as_str() {
                "project" => locksmith::DocumentKind::Project,
                "environment" => locksmith::DocumentKind::Environment,
                _ => {
                    return Err(LockfileError::InvalidDocument(format!(
                        "unknown document kind: {kind}"
                    )));
                }
            };
            Ok(lock.document(kind).map(|doc| {
                doc.as_object()
                    .expect("parsed document")
                    .clone()
                    .into_iter()
                    .collect()
            }))
        })
    }
}

enum Input {
    Yaml(String),
    Parsed(Lockfile),
}

impl From<Either<String, &ParsedLockfile>> for Input {
    fn from(value: Either<String, &ParsedLockfile>) -> Self {
        match value {
            Either::A(yaml) => Self::Yaml(yaml),
            Either::B(parsed) => Self::Parsed(parsed.0.clone()),
        }
    }
}

impl Input {
    fn parse(self) -> Result<Lockfile> {
        match self {
            Self::Yaml(yaml) => Lockfile::try_from(yaml),
            Self::Parsed(lock) => Ok(lock),
        }
    }

    fn merge_input(&self) -> locksmith::MergeInput<'_> {
        match self {
            Self::Yaml(yaml) => yaml.as_str().into(),
            Self::Parsed(lock) => lock.into(),
        }
    }
}

#[napi]
pub fn parse(yaml: String) -> AsyncTask<Job<ParsedLockfile>> {
    job(move || Lockfile::try_from(yaml).map(ParsedLockfile))
}

#[napi]
pub fn validate(lock: Either<String, &ParsedLockfile>) -> AsyncTask<Job<()>> {
    let input = Input::from(lock);
    job(move || input.parse()?.validate())
}

#[napi(object, object_from_js = false)]
pub struct DependencyClosure {
    pub importers: Vec<String>,
    pub packages: Vec<String>,
    pub snapshots: Vec<String>,
}

#[napi]
pub fn dependency_closure(
    lock: Either<String, &ParsedLockfile>,
    importer: String,
) -> AsyncTask<Job<DependencyClosure>> {
    let input = Input::from(lock);
    job(move || {
        let closure = input.parse()?.dependency_closure(&importer)?;
        Ok(DependencyClosure {
            importers: closure.importers.into_iter().collect(),
            packages: closure.packages.into_iter().collect(),
            snapshots: closure.snapshots.into_iter().collect(),
        })
    })
}

#[napi]
pub fn inspect_peers(
    lock: Either<String, &ParsedLockfile>,
    options: Option<Value>,
) -> AsyncTask<Job<Vec<Value>>> {
    let input = Input::from(lock);
    job(move || {
        let options: PeerOptions = decode(options.unwrap_or_else(|| json!({})))?;
        options.inspect(&input.parse()?)
    })
}

#[napi]
pub fn merge(
    base: Either<String, &ParsedLockfile>,
    ours: Either<String, &ParsedLockfile>,
    theirs: Either<String, &ParsedLockfile>,
    context: Option<Value>,
) -> AsyncTask<Job<String>> {
    let inputs = [base, ours, theirs].map(Input::from);
    job(move || {
        let [b, o, t] = inputs.each_ref().map(Input::merge_input);
        let lock = match context {
            Some(raw) => decode::<Merge>(raw)?.apply(b, o, t)?,
            None => locksmith::merge(b, o, t, None)?,
        };
        lock.to_yaml()
    })
}

#[napi]
pub fn converge(
    baseline: Either<String, &ParsedLockfile>,
    candidate: Either<String, &ParsedLockfile>,
    changes: Value,
    options: Option<Value>,
) -> AsyncTask<Job<String>> {
    let [baseline, candidate] = [baseline, candidate].map(Input::from);
    job(move || {
        let changes: Changes = decode(changes)?;
        let options: Converge = decode(options.unwrap_or_else(|| json!({})))?;
        options
            .apply(&baseline.parse()?, &candidate.parse()?, &changes)?
            .to_yaml()
    })
}

#[napi]
pub fn repair(yaml: String, context: Value) -> AsyncTask<Job<String>> {
    job(move || decode::<Repair>(context)?.apply(&yaml)?.to_yaml())
}
