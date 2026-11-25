use std::collections::BTreeSet;

use serde_json::{Map, json};

use crate::{Conflict, ConflictKind, DocumentKind, Lockfile, LockfileError, Value, graph};

/// A parsed lockfile or its original YAML text.
pub enum MergeInput<'a> {
    /// Reuse an already parsed lockfile.
    Parsed(&'a Lockfile),
    /// Parse the supplied YAML as part of the merge.
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
///
/// # Errors
/// Returns [`LockfileError::Conflicts`] for incompatible branch edits, or
/// [`LockfileError::ResolutionRequired`] when the supplied evidence cannot select a pin.
/// Malformed inputs and invalid dependency graphs also return errors.
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
            for (key, value) in context.peers.into_generated() {
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
            if kind == DocumentKind::Project && context.conflicts.is_empty() {
                graph::refresh_merged_optional(
                    &mut document,
                    [ours, theirs].into_iter().filter_map(Lockfile::project),
                )?;
            }
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
        let ordered = path.len() == 1 && path[0] == "overrides";
        let equal = |left: Option<&Value>, right: Option<&Value>| {
            if ordered {
                crate::manifest::same_ordered_mapping(left, right)
            } else {
                left == right
                    && (!path.is_empty()
                        || left
                            .zip(right)
                            .is_none_or(|(a, b)| crate::manifest::same_override_order(a, b)))
            }
        };
        if equal(ours, theirs) {
            return ours.cloned();
        }
        if !container {
            if equal(ours, base) {
                return theirs.cloned();
            }
            if equal(theirs, base) {
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
            let (left, right) = (
                ours.unwrap().as_object().unwrap(),
                theirs.unwrap().as_object().unwrap(),
            );
            let base_map = base.and_then(Value::as_object);
            let names = if ordered {
                override_keys(base_map, left, right)
            } else {
                // Keep the first parent's field order. The writer sorts graph sections.
                let mut seen = BTreeSet::new();
                Some(
                    left.keys()
                        .chain(right.keys())
                        .chain(base_map.into_iter().flat_map(Map::keys))
                        .filter(|key| seen.insert(*key))
                        .cloned()
                        .collect(),
                )
            };
            let Some(names) = names else {
                self.conflicts.push(Conflict {
                    document: self.kind,
                    path: path.clone(),
                    kind: ConflictKind::BothChanged,
                    base: base.cloned(),
                    ours: ours.cloned(),
                    theirs: theirs.cloned(),
                });
                return ours.cloned();
            };
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
        if equal(ours, base) {
            return theirs.cloned();
        }
        if equal(theirs, base) {
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

// Combine independent insertions around shared keys, preserving the order each
// branch authored. A single reordered branch also owns the order when the other
// branch only edited values. Incompatible order edits require a decision.
fn override_keys(
    base: Option<&Map<String, Value>>,
    ours: &Map<String, Value>,
    theirs: &Map<String, Value>,
) -> Option<Vec<String>> {
    if let Some(base) = base {
        for (unchanged, edited) in [(ours, theirs), (theirs, ours)] {
            if unchanged.keys().eq(base.keys()) {
                return Some(
                    edited
                        .keys()
                        .chain(unchanged.keys().filter(|k| !edited.contains_key(*k)))
                        .cloned()
                        .collect(),
                );
            }
        }
    }
    let mut left = ours.keys().peekable();
    let mut right = theirs.keys().peekable();
    let mut keys = Vec::new();
    while left.peek().is_some() || right.peek().is_some() {
        if left.peek().is_some_and(|k| !theirs.contains_key(*k)) {
            keys.push(left.next()?.clone());
        } else if right.peek().is_some_and(|k| !ours.contains_key(*k)) {
            keys.push(right.next()?.clone());
        } else if left.peek() == right.peek() {
            keys.push(left.next()?.clone());
            right.next();
        } else {
            return None;
        }
    }
    Some(keys)
}
