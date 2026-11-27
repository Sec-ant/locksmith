use std::{borrow::Cow, collections::BTreeSet};

use serde_json::{Map, json};

use crate::{Conflict, ConflictKind, DocumentKind, Lockfile, LockfileError, Value, graph, schema};

/// A parsed lockfile or its original YAML text.
#[derive(Clone, Copy, Debug)]
pub enum MergeInput<'a> {
    /// Reuse an already parsed lockfile.
    Parsed(&'a Lockfile),
    /// Parse the supplied YAML as part of the merge.
    Yaml(&'a str),
}

impl<'a> MergeInput<'a> {
    pub(crate) fn bootstrap(self) -> (Option<Cow<'a, Lockfile>>, Vec<String>) {
        match self {
            Self::Parsed(lock) => (Some(Cow::Borrowed(lock)), Vec::new()),
            Self::Yaml(text) => crate::recovery::bootstrap_lockfile(text)
                .map(|(lock, duplicates)| (Some(Cow::Owned(lock)), duplicates))
                .unwrap_or_default(),
        }
    }

    /// Read the recorded runner from the bootstrap document, including recoverable YAML inputs.
    #[must_use]
    pub fn recorded_pnpm_version(self) -> Option<node_semver::Version> {
        self.bootstrap()
            .0
            .as_deref()
            .and_then(Lockfile::recorded_pnpm_version)
    }

    /// Return explicitly declared tarballs whose matching bootstrap release omits its URL.
    /// Adapters need registry settings only for these packages.
    #[must_use]
    pub fn implicit_config_tarballs(self, declarations: &Map<String, Value>) -> Vec<&str> {
        let (lock, _) = self.bootstrap();
        let Some(env) = lock.as_deref().and_then(Lockfile::environment) else {
            return Vec::new();
        };
        let Some(entries) = env["importers"]["."]["configDependencies"].as_object() else {
            return Vec::new();
        };
        if declarations.len() != entries.len() {
            return Vec::new();
        }
        declarations
            .iter()
            .filter_map(|(name, dep)| {
                dep.get("tarball")?.as_str()?;
                let (version, integrity) = dep.get("integrity")?.as_str()?.split_once('+')?;
                let resolution = crate::config::locked_config_resolution(
                    env,
                    name,
                    entries.get(name)?,
                    version,
                    integrity,
                )?;
                resolution.get("tarball").is_none().then_some(name.as_str())
            })
            .collect()
    }
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

/// The three inputs of a contextual merge, including the bootstrap graph selected
/// while loading the final workspace. Reuse this value when adding registry evidence.
#[derive(Debug, Clone, Copy)]
pub struct ContextualMerge<'a> {
    pub(crate) inputs: [MergeInput<'a>; 3],
    pub(crate) bootstrap: Option<usize>,
}

impl ContextualMerge<'_> {
    /// Merge these inputs against the supplied workspace.
    ///
    /// # Errors
    /// Returns the contextual errors described by [`merge`].
    pub fn merge(&self, context: &crate::MergeContext<'_>) -> Result<Lockfile, LockfileError> {
        match self.inputs {
            [
                MergeInput::Parsed(b),
                MergeInput::Parsed(o),
                MergeInput::Parsed(t),
            ] => crate::reconcile::resolve([b, o, t], context, self.bootstrap),
            inputs => crate::recovery::resolve(inputs, context, self.bootstrap),
        }
    }

    /// Collect independent registry requests when the inputs have already been parsed.
    ///
    /// # Errors
    /// Returns the errors of [`Self::merge`]. An error other than missing registry evidence
    /// is reported on its own.
    pub fn merge_with_requests(
        &self,
        context: &crate::MergeContext<'_>,
    ) -> Result<Lockfile, Vec<LockfileError>> {
        match self.inputs {
            [
                MergeInput::Parsed(b),
                MergeInput::Parsed(o),
                MergeInput::Parsed(t),
            ] => crate::reconcile::resolve_with_requests([b, o, t], context, self.bootstrap),
            _ => self.merge(context).map_err(|error| vec![error]),
        }
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
/// [`LockfileError::ResolutionRequired`] when the supplied evidence cannot select compatible pins or settings.
/// Malformed inputs and invalid dependency graphs also return errors.
pub fn merge<'a>(
    base: impl Into<MergeInput<'a>>,
    ours: impl Into<MergeInput<'a>>,
    theirs: impl Into<MergeInput<'a>>,
    context: Option<&crate::MergeContext<'_>>,
) -> Result<Lockfile, LockfileError> {
    let inputs = [base.into(), ours.into(), theirs.into()];
    if let Some(context) = context {
        return ContextualMerge {
            inputs,
            bootstrap: None,
        }
        .merge(context);
    }
    let parse = |input| match input {
        MergeInput::Parsed(lock) => Ok(Cow::Borrowed(lock)),
        MergeInput::Yaml(text) => Lockfile::parse(text).map(Cow::Owned),
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
    #[cfg(feature = "bench-profile")]
    let _span = crate::profile::span("three_way_merge");
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
            graph::complete(&mut document, kind, None, &mut context.conflicts)?;
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
                schema::same_ordered_mapping(left, right)
            } else {
                left == right
                    && (!path.is_empty()
                        || left
                            .zip(right)
                            .is_none_or(|(a, b)| schema::same_override_order(a, b)))
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
                let right_only = right.keys().filter(|key| !left.contains_key(*key));
                let base_only = base_map
                    .into_iter()
                    .flat_map(Map::keys)
                    .filter(|key| !left.contains_key(*key) && !right.contains_key(*key));
                Some(
                    left.keys()
                        .chain(right_only)
                        .chain(base_only)
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

// Merge precedence among surviving base rules separately from insertions.
// A deletion still participates in the value merge so delete/modify conflicts surface.
fn override_keys(
    base: Option<&Map<String, Value>>,
    ours: &Map<String, Value>,
    theirs: &Map<String, Value>,
) -> Option<Vec<String>> {
    if ours.keys().eq(theirs.keys()) {
        return Some(ours.keys().cloned().collect());
    }
    let mut names = Vec::new();
    let mut indices = std::collections::HashMap::new();
    let parents = [ours, theirs].map(|map| {
        map.keys()
            .map(|key| {
                *indices.entry(key.as_str()).or_insert_with(|| {
                    names.push(key);
                    names.len() - 1
                })
            })
            .collect::<Vec<_>>()
    });
    let surviving = |key: &&String| ours.contains_key(*key) && theirs.contains_key(*key);
    let original: Vec<_> = base
        .into_iter()
        .flat_map(Map::keys)
        .filter(surviving)
        .map(|key| indices[key.as_str()])
        .collect();
    let anchors: BTreeSet<_> = original.iter().copied().collect();
    let [left, right] = parents.each_ref().map(|keys| {
        keys.iter()
            .copied()
            .filter(|k| anchors.contains(k))
            .collect::<Vec<_>>()
    });
    let order = if left == right || right == original {
        &left
    } else if left == original {
        &right
    } else {
        return None;
    };

    // Each inserted run keeps its neighbors. Leading and trailing runs stay
    // at their respective ends when the other parent reorders existing rules.
    let mut successors = vec![BTreeSet::new(); names.len()];
    let mut incoming = vec![0; names.len()];
    let mut precedes = |a: usize, b: usize| {
        if successors[a].insert(b) {
            incoming[b] += 1;
        }
    };
    for pair in order.windows(2) {
        precedes(pair[0], pair[1]);
    }
    for mut keys in parents {
        keys.retain(|k| anchors.contains(k) || base.is_none_or(|m| !m.contains_key(names[*k])));
        for pair in keys.windows(2) {
            if !anchors.contains(&pair[0]) || !anchors.contains(&pair[1]) {
                precedes(pair[0], pair[1]);
            }
        }
        if let Some((&first, &last)) = order.first().zip(order.last()) {
            if let Some(&prefix) = keys.iter().take_while(|k| !anchors.contains(k)).last() {
                precedes(prefix, first);
            }
            if let Some(&suffix) = keys
                .iter()
                .rev()
                .take_while(|k| !anchors.contains(k))
                .last()
            {
                precedes(last, suffix);
            }
        }
    }

    // Indices break unconstrained ties in parent order. A cycle means the
    // insertions and the chosen precedence cannot both be honored.
    let mut ready: BTreeSet<_> = (0..names.len()).filter(|&i| incoming[i] == 0).collect();
    let mut result = Vec::with_capacity(names.len());
    while let Some(i) = ready.pop_first() {
        result.push(names[i].clone());
        for &next in &successors[i] {
            incoming[next] -= 1;
            if incoming[next] == 0 {
                ready.insert(next);
            }
        }
    }
    (result.len() == names.len()).then_some(result)
}
