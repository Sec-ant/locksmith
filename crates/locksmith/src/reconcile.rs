//! Resolve the final workspace against locked graphs and supplied release metadata.
mod assembly;
mod dedupe;
mod expand;
mod importers;
pub(crate) mod metadata;
mod preference;
pub(crate) mod published;
mod repair;

use crate::{DocumentKind, Lockfile, LockfileError, Value, graph, manifest, peer};
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
#[derive(Debug, Clone, Copy)]
pub enum MergeTarget {
    /// Prefer the first branch over the second for equally suitable pins.
    Ours,
    /// Prefer the second branch over the first for equally suitable pins.
    Theirs,
}

impl MergeTarget {
    /// Decode branch and final configurations for a contextual merge. Each branch supplies
    /// its own bootstrap version. The preferred branch breaks ties for the final configuration.
    ///
    /// # Errors
    /// Returns the configuration errors described by [`crate::WorkspaceConfig::from_pnpm_context`].
    pub fn workspace_configurations(
        self,
        manifest: Option<&Value>,
        inputs: [&crate::MergeInput<'_>; 3],
        branches: [&Value; 3],
        final_settings: &Value,
    ) -> Result<([crate::WorkspaceConfig; 3], crate::WorkspaceConfig), LockfileError> {
        let versions = inputs.map(|input| match input {
            crate::MergeInput::Parsed(lock) => lock.recorded_pnpm_version(),
            crate::MergeInput::Yaml(yaml) => crate::RepairInput::recorded_pnpm_version(yaml),
        });
        let [base, ours, theirs] = [0, 1, 2].map(|i| {
            crate::WorkspaceConfig::from_pnpm_context(branches[i], manifest, versions[i].clone())
        });
        let preferred = [self.index(), self.other_index(), 0]
            .into_iter()
            .find_map(|i| versions[i].clone());
        let final_config =
            crate::WorkspaceConfig::from_pnpm_context(final_settings, manifest, preferred)?;
        Ok(([base?, ours?, theirs?], final_config))
    }

    fn other_index(self) -> usize {
        match self {
            Self::Ours => Source::Theirs as usize,
            Self::Theirs => Source::Ours as usize,
        }
    }

    fn index(self) -> usize {
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
#[derive(Clone, Copy)]
pub struct MergeContext<'a> {
    /// Published release data supplied by the caller. Locked graphs take precedence.
    pub registry: Option<&'a crate::RegistryMetadata>,
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
}

pub(crate) fn missing(path: &[&str], message: impl Into<String>) -> LockfileError {
    LockfileError::ResolutionRequired {
        request: None,
        path: path.iter().map(|s| (*s).into()).collect(),
        message: message.into(),
    }
}

/// Merge using final manifests/configuration and an explicit target preference.
/// Sources matching the final resolution configuration are preferred, ordered by
/// the target branch. Other inputs supply individually compatible packages when
/// those sources lack a suitable resolution. Peer contexts are
/// rebound to the selected providers. Missing versions or configuration metadata
/// are reported explicitly, without registry access or package-manager calls.
pub(crate) fn resolve(
    base: &Lockfile,
    ours: &Lockfile,
    theirs: &Lockfile,
    context: &MergeContext<'_>,
) -> Result<Lockfile, LockfileError> {
    resolve_sources([base, ours, theirs], context, false, None)
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
    /// The caller can supply the requested registry metadata in one pass.
    ///
    /// # Errors
    /// Returns the errors of [`crate::merge`]. A failure unrelated to missing
    /// registry metadata is reported on its own.
    pub fn merge_with_requests(
        &self,
        locks: [&Lockfile; 3],
    ) -> Result<Lockfile, Vec<LockfileError>> {
        let mut additional = Vec::new();
        resolve_sources(locks, self, false, Some(&mut additional)).map_err(|first| {
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
        let next = resolve_sources([baseline, baseline, &current], context, true, None)?;
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

fn resolve_sources(
    locks: [&Lockfile; 3],
    context: &MergeContext<'_>,
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
    let metadata = metadata::resolve(docs, order, context)?;
    if prefer_baseline {
        crate::repair::check_configuration(docs[2], &metadata, context.configuration)?;
    }
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
            compatibility: crate::config::ResolutionCompatibility::new(
                &config.resolution,
                &context.configuration.resolution,
                doc,
            ),
            matches_configuration,
            unchanged_rules: same_overrides
                && doc.get("patchedDependencies") == metadata.get("patchedDependencies"),
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
        crate::assembly::complete(&mut output, DocumentKind::Project, None, &mut conflicts)?;
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
        let environment = environment_source(
            locks,
            branch_order,
            &context.manifests["."],
            context.configuration,
        )?;
        let mut documents = BTreeMap::new();
        if let Some(env) = locks[environment].documents.get(&DocumentKind::Environment) {
            documents.insert(DocumentKind::Environment, Arc::clone(env));
        }
        documents.insert(DocumentKind::Project, Arc::new(output));
        let mut result = Lockfile::with_documents(
            documents,
            &[locks[target], locks[context.target.other_index()], locks[0]],
        );
        result.preserve_baseline = prefer_baseline;
        result.validate()?;
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

// Package-manager dependencies and their transitive platform binaries form a
// separate lock document. Reuse the complete graph for the declared manager.
fn environment_source(
    locks: [&Lockfile; 3],
    order: [usize; 3],
    manifest: &Value,
    config: &crate::WorkspaceConfig,
) -> Result<usize, LockfileError> {
    let mut matching = order
        .into_iter()
        .filter(|&side| config.matches_config_dependencies(locks[side].environment()));
    let first = matching.next().ok_or_else(|| {
        missing(
            &["importers", ".", "configDependencies"],
            "no bootstrap graph matches the final configuration dependencies",
        )
    })?;
    let mut order = std::iter::once(first).chain(matching);
    let recorded = order
        .clone()
        .find_map(|side| locks[side].recorded_pnpm_version());
    let Some(manager) = manifest::PnpmManager::managed(manifest, Some(config), recorded.as_ref())
    else {
        return Ok(first);
    };
    let managers = locks.map(|lock| lock.matches_pnpm_manager(&manager));
    if let Some(side) = order.clone().find(|&side| managers[side] == Some(true)) {
        return Ok(side);
    }
    if !manager.requires_pin
        && let Some(side) = order.find(|&side| managers[side].is_none())
    {
        return Ok(side);
    }
    Err(missing(
        &["importers", ".", "packageManagerDependencies", "pnpm"],
        format!("no locked bootstrap graph for pnpm@{}", manager.specifier),
    ))
}

fn opaque(specifier: &str) -> bool {
    !specifier.starts_with("catalog:")
        && !specifier.starts_with("npm:")
        && (specifier.contains(':') || specifier.parse::<node_semver::Range>().is_err())
}
pub(crate) fn reference(name: &str, key: &str) -> String {
    key.strip_prefix(name)
        .and_then(|s| s.strip_prefix('@'))
        .unwrap_or(key)
        .into()
}
pub(crate) fn accepts(name: &str, specifier: &str, reference: &str, metadata: &Value) -> bool {
    if let Some(catalog) = specifier.strip_prefix("catalog:") {
        let catalog = if catalog.is_empty() {
            "default"
        } else {
            catalog
        };
        let Some(entry) = metadata
            .get("catalogs")
            .and_then(|v| v.get(catalog))
            .and_then(|v| v.get(name))
        else {
            return false;
        };
        let Some(version) = entry.get("version").and_then(Value::as_str) else {
            return false;
        };
        return catalog_pin_matches(name, entry["specifier"].as_str(), version, reference);
    }
    range_accepts(name, specifier, reference)
}

fn catalog_pin_matches(name: &str, spec: Option<&str>, version: &str, reference: &str) -> bool {
    let pin = graph::package_key(reference);
    if let Some(request) = spec.and_then(|s| s.strip_prefix("workspace:"))
        && let Some(path) = version.strip_prefix("file:")
    {
        return graph::file_pin(name, reference).is_some_and(|(package, pinned_path)| {
            pinned_path == path
                && (request.starts_with('.')
                    || package == crate::workspace_links::workspace_request(name, request).0)
        });
    }
    if let Some((target, _)) = spec.and_then(manifest::alias) {
        return (target == name && pin == version)
            || pin.strip_prefix(target).and_then(|s| s.strip_prefix('@')) == Some(version);
    }
    pin == version
}

fn range_accepts(name: &str, specifier: &str, reference: &str) -> bool {
    let (range, version) = if let Some((target, range)) = manifest::alias(specifier) {
        let Some(version) = reference
            .strip_prefix(&format!("{target}@"))
            .or_else(|| (name == target).then_some(reference))
        else {
            return false;
        };
        (range, graph::package_key(version))
    } else {
        (specifier, graph::package_key(reference))
    };
    manifest::matches_version(range, Some(version))
}
// The outer None denotes competing selectors without a unique applicable patch.
pub(crate) fn desired_patch<'a>(
    key: &str,
    metadata: &'a Value,
    docs: [&Value; 4],
) -> Option<Option<&'a str>> {
    let package = graph::package_key(key);
    let Some(patches) = metadata
        .get("patchedDependencies")
        .and_then(Value::as_object)
    else {
        return Some(None);
    };
    if patches.is_empty() {
        return Some(None);
    }
    // Git, file and tarball locators identify the artifact. pnpm matches patch
    // selectors against its recorded release version, just as for a registry pin.
    let recorded;
    let package = if let Some(separator) = package
        .char_indices()
        .skip(1)
        .find_map(|(i, c)| (c == '@').then_some(i))
        && let (name, locator) = (&package[..separator], &package[separator + 1..])
        && locator.contains(':')
        && let Some(version) = docs
            .iter()
            .find_map(|doc| doc.get("packages")?.get(package)?.get("version")?.as_str())
    {
        recorded = format!("{name}@{version}");
        &recorded
    } else {
        package
    };
    let mut declared = patches.get(package);
    if declared.is_none()
        && let Some((name, version)) = package.rsplit_once('@')
    {
        let version = version.parse::<node_semver::Version>().ok();
        let matching: Vec<_> = patches
            .iter()
            .filter_map(|(selector, patch)| {
                let range = selector
                    .strip_prefix(name)?
                    .strip_prefix('@')?
                    .parse::<node_semver::Range>()
                    .ok()?;
                version
                    .as_ref()
                    .is_some_and(|v| v.satisfies(&range))
                    .then_some((range, patch))
            })
            .collect();
        if matching.is_empty() {
            declared = patches.get(name);
        } else {
            let mut narrowest = matching
                .iter()
                .filter(|(range, _)| matching.iter().all(|(other, _)| other.allows_all(range)))
                .map(|(_, patch)| *patch);
            declared = narrowest.next();
            if declared.is_none() || narrowest.any(|p| Some(p) != declared) {
                return None;
            }
        }
    }
    let declared = declared.and_then(|v| v.as_str().or_else(|| v.get("hash")?.as_str()));
    Some(declared)
}

pub(crate) fn peer_names(doc: &Value, key: &str) -> BTreeSet<String> {
    let mut names: BTreeSet<_> = doc
        .get("packages")
        .and_then(|v| v.get(graph::package_key(key)))
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
    compatibility: crate::config::ResolutionCompatibility<'a>,
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
        self.desired_patch(key, metadata) == Some(peer::patch(key))
    }

    fn desired_patch<'m>(&self, key: &str, metadata: &'m Value) -> Option<Option<&'m str>> {
        desired_patch(key, metadata, self.documents())
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
                    if let Some((name, _)) = graph::package_key(key).rsplit_once('@') {
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
