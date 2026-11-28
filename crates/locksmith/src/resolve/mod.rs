//! Resolve the final workspace against locked graphs and supplied release metadata.
mod assembly;
pub(crate) mod bootstrap;
mod dedupe;
pub(crate) mod extensions;
mod importers;
pub(crate) mod metadata;
mod preference;
pub(crate) mod published;
mod rebuild;
pub(crate) mod recovery;
mod rewrite;

use crate::{
    DocumentKind, Lockfile, LockfileError, Value, model::error::missing, model::graph,
    model::manifest, model::patch, model::pin, model::schema, peer,
};
use serde_json::{Map, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

type PeerRequirements = BTreeMap<String, BTreeSet<String>>;
#[derive(Clone, Copy)]
enum Source {
    Base,
    Ours,
    Theirs,
    Published,
}

impl Source {
    const ALL: [Self; 4] = [Self::Base, Self::Ours, Self::Theirs, Self::Published];
}
const PUBLISHED: usize = Source::Published as usize;

#[derive(Default)]
struct PeerChanges {
    // Requirements introduced by selection, with locked provider evidence.
    required: PeerRequirements,
    // Existing bindings affected by a provider change in their enclosing scope.
    affected: PeerRequirements,
    unresolved: PeerRequirements,
    // Newly supplied local dependencies can retire inherited peer bindings.
    supplied: PeerRequirements,
}
struct RepairedSnapshot {
    body: Value,
    package: Value,
    peers: PeerChanges,
    extended: bool,
}

/// The preferred branch when several locked candidates are equally suitable.
///
/// Each dependency is selected separately. This preference applies to ties
/// throughout the merge, so a single result can contain pins from both branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeTarget {
    /// Prefer the first branch over the second for equally suitable pins.
    Ours,
    /// Prefer the second branch over the first for equally suitable pins.
    Theirs,
}

impl MergeTarget {
    fn other_index(self) -> usize {
        match self {
            Self::Ours => Source::Theirs as usize,
            Self::Theirs => Source::Ours as usize,
        }
    }

    pub(crate) fn index(self) -> usize {
        match self {
            Self::Ours => Source::Ours as usize,
            Self::Theirs => Source::Theirs as usize,
        }
    }
}

/// Final workspace intent. Configurations are normalized workspace data
/// (base, ours, theirs); `configuration` is their resolved final value.
/// Manifests contain every final workspace importer, including the root `.`.
/// Patch contents are keyed by their workspace-relative configured paths.
/// Callers expand workspace globs and read files; the library performs no I/O.
#[derive(Clone, Copy, Debug)]
pub struct MergeContext<'a> {
    /// Branch with priority when compatible pins tie.
    pub target: MergeTarget,
    /// Final package manifests keyed by importer path, including `.`.
    pub manifests: &'a BTreeMap<String, Value>,
    /// Effective configurations for the common ancestor, first branch and second branch, in that order.
    pub configurations: [&'a crate::WorkspaceConfig; 3],
    /// Effective workspace configuration after merging the manifests and settings.
    pub configuration: &'a crate::WorkspaceConfig,
    /// Final patch contents keyed by their declared workspace-relative paths.
    pub patches: &'a BTreeMap<String, String>,
    /// Published release data supplied by the caller. Locked graphs take precedence.
    pub registry: Option<&'a crate::RegistryMetadata>,
}

/// Merge using final manifests/configuration and an explicit target preference.
/// Sources matching the final resolution configuration are preferred, ordered by
/// the target branch. Other inputs supply individually compatible packages when
/// those sources lack a suitable resolution. Peer contexts are
/// rebound to the selected providers. Missing versions or configuration metadata
/// are reported explicitly, without registry access or package-manager calls.
pub(crate) fn resolve(
    locks: [&Lockfile; 3],
    context: &MergeContext<'_>,
    bootstrap: Option<usize>,
) -> Result<Lockfile, LockfileError> {
    resolve_sources(locks, context, Bootstrap::Select(bootstrap), false, None)
}

pub(crate) fn resolve_project(
    locks: [&Lockfile; 3],
    context: &MergeContext<'_>,
) -> Result<Lockfile, LockfileError> {
    resolve_sources(locks, context, Bootstrap::ProjectOnly, false, None)
}

struct MissingRequests<'a> {
    first: Option<LockfileError>,
    additional: Option<&'a mut Vec<LockfileError>>,
}

impl<'a> MissingRequests<'a> {
    fn new(additional: Option<&'a mut Vec<LockfileError>>) -> Self {
        Self {
            first: None,
            additional,
        }
    }

    fn record(&mut self, error: LockfileError) -> Result<(), LockfileError> {
        let Some(additional) = self.additional.as_deref_mut() else {
            return Err(error);
        };
        if !matches!(
            error,
            LockfileError::ResolutionRequired {
                request: Some(_),
                ..
            }
        ) {
            return Err(error);
        }
        if self.first.is_some() {
            additional.push(error);
        } else {
            self.first = Some(error);
        }
        Ok(())
    }

    fn finish(self) -> Result<(), LockfileError> {
        self.first.map_or(Ok(()), Err)
    }
}

impl MergeContext<'_> {
    /// Merge parsed locks and report independent registry requests from the same resolution phase together.
    /// The caller can supply the requested registry metadata in one pass. A workspace loaded
    /// by [`MergeTarget`] can reuse its bootstrap selection through [`crate::ContextualMerge::merge_with_requests`].
    ///
    /// # Errors
    /// Returns the errors of [`crate::merge`]. A failure unrelated to missing
    /// registry metadata is reported on its own.
    pub fn merge_with_requests(
        &self,
        locks: [&Lockfile; 3],
    ) -> Result<Lockfile, Vec<LockfileError>> {
        resolve_with_requests(locks, self, None)
    }
}

pub(crate) fn resolve_with_requests(
    locks: [&Lockfile; 3],
    context: &MergeContext<'_>,
    bootstrap: Option<usize>,
) -> Result<Lockfile, Vec<LockfileError>> {
    let mut additional = Vec::new();
    resolve_sources(
        locks,
        context,
        Bootstrap::Select(bootstrap),
        false,
        Some(&mut additional),
    )
    .map_err(|first| {
        let collect = matches!(
            first,
            LockfileError::ResolutionRequired {
                request: Some(_),
                ..
            }
        );
        let mut errors = vec![first];
        if collect {
            errors.append(&mut additional);
        }
        errors
    })
}

pub(crate) fn converge_upgrade(
    baseline: &Lockfile,
    installed: &Lockfile,
    context: &MergeContext<'_>,
) -> Result<Lockfile, LockfileError> {
    use sha2::{Digest, Sha256};
    let mut current = std::borrow::Cow::Borrowed(installed);
    let mut seen = BTreeSet::new();
    loop {
        let next = resolve_sources(
            [baseline, baseline, &current],
            context,
            Bootstrap::Select(Some(Source::Theirs as usize)),
            true,
            None,
        )?;
        if next.documents == current.documents {
            return Ok(next);
        }
        // Rebinding one instance can expose another admissible baseline pin.
        // Detect recurrence without retaining a large dependency graph per pass.
        let mut hash = Sha256::new();
        serde_json::to_writer(&mut hash, &next.project()).expect("JSON values can be hashed");
        if !seen.insert(hash.finalize()) {
            return Err(missing(
                &["snapshots"],
                "pnpm migration did not reach stable peer bindings",
            ));
        }
        current = std::borrow::Cow::Owned(next);
    }
}

enum Bootstrap {
    Select(Option<usize>),
    ProjectOnly,
}

fn resolve_sources(
    locks: [&Lockfile; 3],
    context: &MergeContext<'_>,
    bootstrap: Bootstrap,
    prefer_baseline: bool,
    mut additional: Option<&mut Vec<LockfileError>>,
) -> Result<Lockfile, LockfileError> {
    if !context.manifests.contains_key(".") {
        return Err(missing(
            &["manifests", "."],
            "the root manifest is required",
        ));
    }
    for (id, manifest) in context.manifests {
        if !manifest.is_object() {
            return Err(LockfileError::InvalidManifest(id.clone()));
        }
    }
    let docs = locks.map(Lockfile::project);
    let docs = docs.map(|d| d.ok_or_else(|| missing(&[], "missing project document")));
    let [b, o, t] = docs;
    let docs = [b?, o?, t?];
    let mut order = [
        context.target.index(),
        context.target.other_index(),
        Source::Base as usize,
    ];
    let target = order[0];
    let matching_configuration = context.configurations.map(|source| {
        source.matches_pnpm(context.configuration)
            && source.resolution == context.configuration.resolution
    });
    // Reuse a coherent resolution before mixing individually compatible records
    // from other configurations. This avoids unnecessary peer-context changes.
    order.sort_by_key(|&side| {
        (
            !matching_configuration[side],
            !context.configurations[side].matches_pnpm(context.configuration),
        )
    });
    let preferred = order[0];
    let settings_source = if prefer_baseline {
        preferred
    } else {
        // A compatible header can come from a different source than the preferred pins.
        match metadata::check_merge_settings(docs[preferred], context.configuration) {
            Ok(()) => preferred,
            Err(error) => order[1..]
                .iter()
                .copied()
                .find(|&side| {
                    metadata::check_merge_settings(docs[side], context.configuration).is_ok()
                })
                .ok_or(error)?,
        }
    };
    let metadata = metadata::resolve(docs, order, settings_source, context)?;
    if prefer_baseline {
        metadata::check_repair_configuration(docs[2], &metadata, context.configuration)?;
    }
    let environment = match bootstrap {
        Bootstrap::Select(selected) => Some(bootstrap::environment_source(
            locks.map(Some),
            context.target,
            context.manifests.get("."),
            |side| {
                selected
                    .is_none_or(|selected| selected == side)
                    .then_some(context.configuration)
            },
            [&[]; 3],
        )?),
        Bootstrap::ProjectOnly => None,
    };
    let evidence = published::records(context, &metadata, docs, order, additional.as_deref_mut())?;
    let branch_order = order;
    // Migration convergence keeps admissible baseline pins. Headers and the
    // bootstrap graph still come from the final package manager.
    if prefer_baseline {
        order = [Source::Ours, Source::Theirs, Source::Base].map(|source| source as usize);
    }
    let [first, second, third] = order;
    let order = [first, second, third, PUBLISHED];
    let final_override_document = json!({"overrides":metadata.get("overrides")});
    let final_rules = manifest::Overrides::new(&final_override_document, context.configuration);
    let inputs = Source::ALL.map(|id| {
        let (doc, config, matches_configuration) = match id {
            Source::Published => (&evidence, context.configuration, true),
            branch => {
                let i = branch as usize;
                (
                    docs[i],
                    context.configurations[i],
                    matching_configuration[i],
                )
            }
        };
        let rules = manifest::Overrides::new(doc, config);
        let same_overrides = rules.same_rules(&final_rules);
        GraphSource {
            document: doc,
            rules,
            same_overrides,
            compatibility: crate::model::config::ResolutionCompatibility::new(
                &config.resolution,
                &context.configuration.resolution,
                doc,
            ),
            matches_configuration,
            unchanged_rules: same_overrides
                && patch::records_match(doc, schema::rule_map(&metadata, "patchedDependencies")),
            tie_break: config.override_tie_break,
        }
    });
    let mut source_order = order;
    if !prefer_baseline {
        source_order.sort_by_key(|side| !inputs[*side].same_overrides);
    }
    let unqualified: Map<_, _> = graph::section(&metadata, "overrides")
        .into_iter()
        .flatten()
        .filter(|(rule, _)| manifest::override_selector(rule).2.is_none())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let unqualified = json!({"overrides": unqualified});
    let sources = GraphSources {
        prefer_baseline,
        inputs,
        final_rules,
        unqualified_rules: manifest::Overrides::new(&unqualified, context.configuration),
        order,
        candidates: std::cell::OnceCell::new(),
        registry: context.registry,
        configuration: context.configuration,
        override_tie_break: context.configuration.override_tie_break,
    };
    // All attempts use the same input indexes and final configuration. A retry
    // changes only the preferences and the graph assembled from them.
    let peer_index = peer::PeerIndex::new(
        sources
            .documents()
            .map(|doc| (!doc.is_null()).then_some(doc)),
    );
    let attempt = |preferences: &preference::Preferences,
                   additional: Option<&mut Vec<LockfileError>>| {
        let mut output = metadata.clone();
        let mut peers = peer::Merger::with_index(peer_index.clone());
        peers.configure_context(
            order,
            context.configuration.resolution.lock.dedupe_peers,
            context.configuration.linked_peer_encoding,
        );
        let selected = importers::select(
            context,
            &output,
            &sources,
            &mut peers,
            preferences,
            additional,
        )?;
        output["importers"] = Value::Object(selected.records);
        let extended = assembly::assemble(
            &mut output,
            &sources,
            &mut peers,
            &selected.configured_sources,
            source_order,
            preferences,
            context.target.other_index(),
        )?;
        drop(peers);
        metadata::retain_used_catalogs(&mut output);
        dedupe::reconcile(&mut output, &sources)?;
        // pnpm omits empty graph sections, including when a frozen install writes
        // an empty workspace lockfile. Keep that representation byte-stable.
        for section in ["snapshots", "packages"] {
            if graph::section(&output, section).is_some_and(Map::is_empty) {
                output.as_object_mut().unwrap().remove(section);
            }
        }
        let mut conflicts = Vec::new();
        graph::complete(&mut output, DocumentKind::Project, None, &mut conflicts)?;
        if let Some(times) = metadata::publication_times(
            &output,
            branch_order.map(|i| docs[i]),
            Some(context.configuration),
        ) {
            output["time"] = times;
        }
        if extended || context.configuration.auto_dedupe {
            graph::refresh_optional(&mut output)?;
        } else {
            graph::refresh_merged_optional(&mut output, docs)?;
        }
        let overrides = manifest::Overrides::new(&output, context.configuration);
        for (id, value) in context.manifests {
            manifest::check(&overrides, id, Some(value), "merged")?;
        }
        let mut documents = BTreeMap::new();
        if let Some(env) =
            environment.and_then(|side| locks[side].documents.get(&DocumentKind::Environment))
        {
            documents.insert(DocumentKind::Environment, Arc::clone(env));
        }
        documents.insert(DocumentKind::Project, Arc::new(output));
        let mut result = Lockfile::with_documents(
            documents,
            &[locks[target], locks[context.target.other_index()], locks[0]],
        );
        result.preserve_baseline = prefer_baseline;
        result.validate_with_config(Some(context.configuration))?;
        Ok(result)
    };
    let mut preferences = preference::Preferences::default();
    let branches = docs;
    loop {
        if let Some(errors) = additional.as_deref_mut() {
            errors.clear();
        }
        match attempt(&preferences, additional.as_deref_mut()) {
            Err(error) if preferences.extend(&error, branches, context) => continue,
            result => return result,
        }
    }
}

fn opaque(specifier: &str) -> bool {
    !specifier.starts_with("catalog:")
        && !specifier.starts_with("npm:")
        && (specifier.contains(':') || specifier.parse::<node_semver::Range>().is_err())
}
pub(crate) fn peer_names(doc: &Value, key: &str) -> BTreeSet<String> {
    let mut names: BTreeSet<_> = doc
        .get("packages")
        .and_then(|v| v.get(pin::package_key(key)))
        .and_then(|v| v.get("peerDependencies"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(n, _)| n.clone())
        .collect();
    names.extend(
        doc.get("snapshots")
            .and_then(|v| v.get(key))
            .and_then(|v| v.get("transitivePeerDependencies"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned),
    );
    names
}
type PackageCandidates<'a> = HashMap<&'a str, Vec<(usize, &'a str)>>;

struct GraphSource<'a> {
    document: &'a Value,
    rules: manifest::Overrides<'a>,
    compatibility: crate::model::config::ResolutionCompatibility<'a>,
    same_overrides: bool,
    matches_configuration: bool,
    unchanged_rules: bool,
    tie_break: crate::OverrideTieBreak,
}

struct GraphSources<'a> {
    prefer_baseline: bool,
    registry: Option<&'a crate::RegistryMetadata>,
    inputs: [GraphSource<'a>; 4],
    final_rules: manifest::Overrides<'a>,
    unqualified_rules: manifest::Overrides<'a>,
    order: [usize; 4],
    candidates: std::cell::OnceCell<PackageCandidates<'a>>,
    configuration: &'a crate::WorkspaceConfig,
    override_tie_break: crate::OverrideTieBreak,
}

impl<'a> GraphSources<'a> {
    fn authorized_patch(&self, key: &str, metadata: &Value) -> bool {
        self.desired_patch(key, metadata) == Ok(patch::hash_in_key(key))
    }

    fn desired_patch<'m>(
        &self,
        key: &str,
        metadata: &'m Value,
    ) -> Result<Option<&'m str>, patch::CompetingPatches> {
        patch::desired_hash(key, metadata, self.documents())
    }

    fn documents(&self) -> [&'a Value; 4] {
        self.inputs.each_ref().map(|input| input.document)
    }

    fn candidates(&self) -> &PackageCandidates<'a> {
        self.candidates.get_or_init(|| {
            let mut candidates: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
            for side in self.order {
                for key in graph::section(self.inputs[side].document, "snapshots")
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.as_str())
                {
                    if !self.inputs[side].compatibility.accepts(key) {
                        continue;
                    }
                    if let Some((name, _)) = pin::package_key(key).rsplit_once('@') {
                        candidates.entry(name).or_default().push((side, key));
                    }
                }
            }
            for records in candidates.values_mut() {
                records.sort_by_key(|(side, key)| {
                    (
                        *side == PUBLISHED,
                        !self.inputs[*side].compatibility.unchanged(key),
                    )
                });
            }
            candidates
        })
    }
}
