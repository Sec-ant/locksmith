use std::collections::BTreeSet;

use serde_json::{Map, json};

use crate::{Conflict, ConflictKind, DocumentKind, Lockfile, LockfileError, Value, graph};

/// A parsed lockfile or its original YAML text.
pub enum MergeInput<'a> {
    Parsed(&'a Lockfile),
    Yaml(&'a str),
}

impl<'a> From<&'a Lockfile> for MergeInput<'a> {
    fn from(lock: &'a Lockfile) -> Self {
        Self::Parsed(lock)
    }
}

impl<'a> From<&'a str> for MergeInput<'a> {
    fn from(text: &'a str) -> Self {
        Self::Yaml(text)
    }
}

/// Merge two branches using their common ancestor. With context, resolve pins and
/// peers against the final manifests and workspace settings. Without context,
/// apply independent changes and report reachable disagreements.
///
/// Pass parsed lockfiles or YAML slices. Contextual merges of raw YAML can recover
/// duplicate snapshot keys when every interpretation yields the same result.
pub fn merge<'a>(
    base: impl Into<MergeInput<'a>>,
    ours: impl Into<MergeInput<'a>>,
    theirs: impl Into<MergeInput<'a>>,
    context: Option<&crate::MergeContext<'_>>,
) -> Result<Lockfile, LockfileError> {
    let inputs = [base.into(), ours.into(), theirs.into()];
    if let Some(context) = context {
        return match inputs {
            [
                MergeInput::Parsed(b),
                MergeInput::Parsed(o),
                MergeInput::Parsed(t),
            ] => crate::reconcile::resolve(b, o, t, context),
            inputs => crate::recovery::resolve(inputs, context),
        };
    }
    let parse = |input| match input {
        MergeInput::Parsed(lock) => Ok(std::borrow::Cow::Borrowed(lock)),
        MergeInput::Yaml(text) => Lockfile::parse(text).map(std::borrow::Cow::Owned),
    };
    let [b, o, t] = inputs;
    three_way(&*parse(b)?, &*parse(o)?, &*parse(t)?)
}

/// Three-way merge, with `base` as the common ancestor.
///
/// Equal changes and changes made by only one side are accepted. Catalog entries,
/// package metadata and patch records are atomic. Snapshot dependencies merge by
/// edge. Independent peer-context changes compose into checked instances using
/// versions already present in the inputs. Changed registry declarations are
/// checked against the combined resolution.
/// Graph records deleted by one side remain available until reachability pruning,
/// since the other side may add a new reference to an unchanged record.
/// No merged lockfile is returned when a reachable conflict remains.
fn three_way(
    base: &Lockfile,
    ours: &Lockfile,
    theirs: &Lockfile,
) -> Result<Lockfile, LockfileError> {
    let mut documents = std::collections::BTreeMap::new();
    let mut conflicts = Vec::new();
    for kind in [DocumentKind::Environment, DocumentKind::Project] {
        let mut context = Context {
            kind,
            conflicts: Vec::new(),
            peers: crate::peer::Merger::new([
                base.document(kind),
                ours.document(kind),
                theirs.document(kind),
            ]),
        };
        let document = context.value(
            &mut Vec::new(),
            base.document(kind),
            ours.document(kind),
            theirs.document(kind),
        );
        if let Some(mut document) = document {
            for (key, value) in std::mem::take(&mut context.peers.generated) {
                let snapshots = document
                    .as_object_mut()
                    .expect("document mapping")
                    .entry("snapshots")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .expect("snapshots mapping");
                if let Some(existing) = snapshots.get(&key) {
                    if existing != &value {
                        context.conflicts.push(Conflict {
                            document: kind,
                            path: vec!["snapshots".into(), key],
                            kind: ConflictKind::BothChanged,
                            base: None,
                            ours: Some(existing.clone()),
                            theirs: Some(value),
                        });
                    }
                } else {
                    snapshots.insert(key, value);
                }
            }
            crate::assembly::complete(&mut document, kind, None, &mut context.conflicts)?;
            documents.insert(kind, std::sync::Arc::new(document));
        }
        conflicts.extend(context.conflicts);
    }
    if !conflicts.is_empty() {
        return Err(LockfileError::Conflicts(conflicts));
    }
    if documents.is_empty() {
        return Err(LockfileError::InvalidDocument(
            "merge deleted all lockfile documents".into(),
        ));
    }
    Ok(Lockfile::with_documents(documents, &[ours, theirs, base]))
}

struct Context<'a> {
    kind: DocumentKind,
    conflicts: Vec<Conflict>,
    peers: crate::peer::Merger<'a>,
}

fn graph_record(path: &[String]) -> bool {
    path.len() == 2 && ["packages", "snapshots"].contains(&path[0].as_str())
}

fn atomic(path: &[String]) -> bool {
    graph_record(path)
        || (path.len() == 4
            && path[0] == "importers"
            && graph::DEPENDENCIES.contains(&path[2].as_str()))
        || (path.len() == 3 && path[0] == "catalogs")
        || (path.len() == 2 && path[0] == "patchedDependencies")
}

impl Context<'_> {
    fn value(
        &mut self,
        path: &mut Vec<String>,
        base: Option<&Value>,
        ours: Option<&Value>,
        theirs: Option<&Value>,
    ) -> Option<Value> {
        if graph_record(path) && (ours.is_none() || theirs.is_none()) {
            return ours.or(theirs).cloned();
        }
        // Always visit graph maps, even when one map is unchanged. Otherwise a
        // deletion could remove an entry that a new importer still references.
        let container = path.is_empty()
            || (path.len() == 1 && ["packages", "snapshots"].contains(&path[0].as_str()));
        if !container || ours == theirs {
            if ours == theirs {
                return ours.cloned();
            }
            if ours == base {
                return theirs.cloned();
            }
            if theirs == base {
                return ours.cloned();
            }
        }
        if path.len() == 2
            && path[0] == "snapshots"
            && let Some(value) = self.peers.snapshot(&path[1], base, ours, theirs)
        {
            return value;
        }
        if path.len() == 4
            && path[0] == "importers"
            && graph::DEPENDENCIES.contains(&path[2].as_str())
            && let Some(value) = self.peers.dependency(&path[3], base, ours, theirs)
        {
            return Some(value);
        }
        let recurse = !atomic(path)
            && ours.is_some_and(Value::is_object)
            && theirs.is_some_and(Value::is_object)
            && base.is_none_or(Value::is_object);
        if recurse {
            let mut result = Map::new();
            // Keep the first parent's root/field order. Entry sections are sorted
            // when emitted, matching pnpm's output.
            let mut seen = BTreeSet::new();
            let names: Vec<_> = ours
                .into_iter()
                .chain(theirs)
                .chain(base)
                .flat_map(|value| value.as_object().expect("mapping was checked").keys())
                .filter(|key| seen.insert((*key).clone()))
                .cloned()
                .collect();
            for name in names {
                path.push(name.clone());
                let value = self.value(
                    path,
                    base.and_then(|v| v.get(&name)),
                    ours.and_then(|v| v.get(&name)),
                    theirs.and_then(|v| v.get(&name)),
                );
                path.pop();
                if let Some(value) = value {
                    result.insert(name, value);
                }
            }
            return Some(Value::Object(result));
        }
        // A graph section can be absent in an otherwise nonempty document.
        if path.len() == 1 && ["packages", "snapshots"].contains(&path[0].as_str()) {
            let empty = json!({});
            return self.value(
                path,
                base,
                Some(ours.unwrap_or(&empty)),
                Some(theirs.unwrap_or(&empty)),
            );
        }
        if ours == base {
            return theirs.cloned();
        }
        if theirs == base {
            return ours.cloned();
        }
        self.conflicts.push(Conflict {
            document: self.kind,
            path: path.clone(),
            kind: if base.is_some() && (ours.is_none() || theirs.is_none()) {
                ConflictKind::DeleteModify
            } else {
                ConflictKind::BothChanged
            },
            base: base.cloned(),
            ours: ours.cloned(),
            theirs: theirs.cloned(),
        });
        // Only an internal candidate used to identify unreachable graph conflicts.
        // Conflicted candidates never escape through the public API.
        ours.or(theirs).cloned()
    }
}
