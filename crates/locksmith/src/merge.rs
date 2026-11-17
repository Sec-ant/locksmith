use std::collections::BTreeSet;

use serde_json::{Map, json};

use crate::{Conflict, ConflictKind, DocumentKind, Lockfile, LockfileError, Value, graph};

/// Three-way merge, with `base` as the common ancestor.
///
/// Equal changes and changes made by only one side are accepted. Importer
/// dependency records, catalog entries, package records, and snapshot records are
/// atomic: a merge never manufactures a new specifier/version or peer binding.
/// Graph records deleted by one side remain available until reachability pruning,
/// since the other side may add a new reference to an unchanged record.
/// No merged lockfile is returned when a reachable conflict remains.
pub fn merge(
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
        };
        let document = context.value(
            &mut Vec::new(),
            base.document(kind),
            ours.document(kind),
            theirs.document(kind),
        );
        if let Some(mut document) = document {
            crate::graft::complete(&mut document, kind, None, &mut context.conflicts)?;
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

struct Context {
    kind: DocumentKind,
    conflicts: Vec<Conflict>,
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

impl Context {
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
