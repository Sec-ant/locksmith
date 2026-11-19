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
const PUBLISHED: usize = 3;
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
    Ours,
    Theirs,
}

impl MergeTarget {
    fn other_index(self) -> usize {
        3 - self.index()
    }

    fn index(self) -> usize {
        match self {
            Self::Ours => 1,
            Self::Theirs => 2,
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
    pub target: MergeTarget,
    pub manifests: &'a BTreeMap<String, Value>,
    pub configurations: [&'a crate::WorkspaceConfig; 3],
    pub configuration: &'a crate::WorkspaceConfig,
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
    let locks = [base, ours, theirs];
    let docs = [base.project(), ours.project(), theirs.project()];
    let docs = docs.map(|d| d.ok_or_else(|| missing(&[], "missing project document")));
    let [b, o, t] = docs;
    let docs = [b?, o?, t?];
    let mut order = match context.target {
        MergeTarget::Ours => [1, 2, 0],
        MergeTarget::Theirs => [2, 1, 0],
    };
    let target = order[0];
    let matching_configuration = context
        .configurations
        .map(|source| source.resolution == context.configuration.resolution);
    // Reuse a coherent resolution before mixing individually compatible records
    // from other configurations. This avoids unnecessary peer-context changes.
    order.sort_by_key(|side| !matching_configuration[*side]);
    let metadata = metadata::resolve(docs, order, context)?;
    let evidence = published::records(context, &metadata, docs, order)?;
    let branch_order = order;
    let [first, second, third] = order;
    let order = [first, second, third, 3];
    let [b, o, t] = docs;
    let docs = [b, o, t, &evidence];
    let [b, o, t] = context.configurations;
    let configs = [b, o, t, context.configuration];
    let [b, o, t] = matching_configuration;
    let matching_configuration = [b, o, t, true];
    let compatibility = std::array::from_fn(|side| {
        crate::config::ResolutionCompatibility::new(
            &configs[side].resolution,
            &context.configuration.resolution,
            docs[side],
        )
    });
    let final_override_document = json!({"overrides":metadata.get("overrides")});
    let final_rules = manifest::Overrides::new(
        &final_override_document,
        context.configuration.override_tie_break,
    );
    let rules = std::array::from_fn(|side| {
        manifest::Overrides::new(docs[side], configs[side].override_tie_break)
    });
    let matching_overrides: [bool; 4] =
        std::array::from_fn(|side| rules[side].same_rules(&final_rules));
    let mut source_order = order;
    source_order.sort_by_key(|side| !matching_overrides[*side]);
    let unqualified: Map<_, _> = graph::section(&metadata, "overrides")
        .into_iter()
        .flatten()
        .filter(|(rule, _)| manifest::override_selector(rule).2.is_none())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let unqualified = json!({"overrides": unqualified});
    let sources = GraphSources {
        docs,
        rules,
        final_rules,
        unqualified_rules: manifest::Overrides::new(
            &unqualified,
            context.configuration.override_tie_break,
        ),
        order,
        candidates: std::cell::OnceCell::new(),
        registry: context.registry,
        compatibility: &compatibility,
        configuration: context.configuration,
        unchanged_configuration: matching_configuration,
        unchanged_rules: std::array::from_fn(|side| {
            matching_overrides[side]
                && docs[side].get("patchedDependencies") == metadata.get("patchedDependencies")
        }),
        override_tie_break: context.configuration.override_tie_break,
        source_tie_breaks: configs.map(|config| config.override_tie_break),
    };
    // All attempts use the same input indexes and final configuration. A retry
    // changes only the preferences and the graph assembled from them.
    let peer_index = peer::PeerIndex::new(docs.map(|doc| (!doc.is_null()).then_some(doc)));
    let attempt = |preferences: &preference::Preferences| {
        let mut output = metadata.clone();
        let mut peers = peer::Merger::with_index(peer_index.clone());
        peers.configure_context(
            order,
            context.configuration.resolution.lock.dedupe_peers,
            context.configuration.linked_peer_encoding,
        );
        let selected = importers::select(context, &output, &sources, &mut peers, preferences)?;
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
        metadata::publish_times(&mut output, [docs[0], docs[1], docs[2]], branch_order);
        if extended || context.configuration.auto_dedupe {
            graph::refresh_optional(&mut output)?;
        }
        let overrides = manifest::Overrides::new(&output, context.configuration.override_tie_break);
        for (id, value) in context.manifests {
            manifest::check(&overrides, id, Some(value), "merged")?;
        }
        let environment = environment_source(locks, branch_order, &context.manifests["."])?;
        let mut documents = BTreeMap::new();
        if let Some(env) = locks[environment].documents.get(&DocumentKind::Environment) {
            documents.insert(DocumentKind::Environment, Arc::clone(env));
        }
        documents.insert(DocumentKind::Project, Arc::new(output));
        let result = Lockfile::with_documents(
            documents,
            &[locks[target], locks[context.target.other_index()], locks[0]],
        );
        result.validate()?;
        Ok(result)
    };
    let mut preferences = preference::Preferences::default();
    let branches = [docs[0], docs[1], docs[2]];
    loop {
        match attempt(&preferences) {
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
) -> Result<usize, LockfileError> {
    let managers = locks.map(|l| {
        l.environment()?
            .get("importers")?
            .get(".")?
            .get("packageManagerDependencies")
    });
    let Some(version) = manifest
        .get("packageManager")
        .and_then(Value::as_str)
        .and_then(|v| v.strip_prefix("pnpm@"))
    else {
        return Ok(order[0]);
    };
    if managers.iter().all(|m| m.is_none()) {
        return Ok(order[0]);
    }
    let version = version.split_once("+sha").map_or(version, |(v, _)| v);
    for side in order {
        if managers[side].is_some_and(|deps| deps["pnpm"]["specifier"] == version) {
            return Ok(side);
        }
    }
    // Older pnpm versions can lack a bootstrap document entirely.
    if version
        .parse::<node_semver::Version>()
        .is_ok_and(|v| v.major < 11)
        && let Some(side) = order.into_iter().find(|i| managers[*i].is_none())
    {
        return Ok(side);
    }
    Err(missing(
        &["importers", ".", "packageManagerDependencies", "pnpm"],
        format!("no locked bootstrap graph for pnpm@{version}"),
    ))
}

pub(crate) fn alias(specifier: &str) -> Option<(&str, &str)> {
    let alias = specifier.strip_prefix("npm:")?;
    Some(
        alias
            .rsplit_once('@')
            .filter(|(n, _)| !n.is_empty())
            .unwrap_or((alias, "*")),
    )
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
    if let Some((target, _)) = spec.and_then(alias) {
        return (target == name && pin == version)
            || pin.strip_prefix(target).and_then(|s| s.strip_prefix('@')) == Some(version);
    }
    pin == version
}

fn range_accepts(name: &str, specifier: &str, reference: &str) -> bool {
    let (range, version) = if let Some((target, range)) = alias(specifier) {
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
fn authorized_patch(key: &str, metadata: &Value, docs: [&Value; 4]) -> bool {
    desired_patch(key, metadata, docs) == Some(peer::patch(key))
}

// The outer None denotes competing selectors without a unique applicable patch.
fn desired_patch<'a>(key: &str, metadata: &'a Value, docs: [&Value; 4]) -> Option<Option<&'a str>> {
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

struct GraphSources<'a> {
    registry: Option<&'a crate::RegistryMetadata>,
    docs: [&'a Value; 4],
    rules: [manifest::Overrides<'a>; 4],
    final_rules: manifest::Overrides<'a>,
    unqualified_rules: manifest::Overrides<'a>,
    order: [usize; 4],
    candidates: std::cell::OnceCell<PackageCandidates<'a>>,
    compatibility: &'a [crate::config::ResolutionCompatibility<'a>; 4],
    configuration: &'a crate::WorkspaceConfig,
    unchanged_configuration: [bool; 4],
    unchanged_rules: [bool; 4],
    override_tie_break: crate::OverrideTieBreak,
    source_tie_breaks: [crate::OverrideTieBreak; 4],
}

impl<'a> GraphSources<'a> {
    fn candidates(&self) -> &PackageCandidates<'a> {
        self.candidates.get_or_init(|| {
            let mut candidates: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
            for side in self.order {
                for key in graph::section(self.docs[side], "snapshots")
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.as_str())
                {
                    if !self.compatibility[side].accepts(key) {
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
                        !self.compatibility[*side].unchanged(key),
                    )
                });
            }
            candidates
        })
    }
}

fn key_in_inputs(
    name: &str,
    reference: &str,
    docs: [&Value; 4],
    generated: &BTreeMap<String, Value>,
) -> String {
    let named = format!("{name}@{reference}");
    if generated.contains_key(&named)
        || docs
            .iter()
            .any(|d| d.get("snapshots").and_then(|v| v.get(&named)).is_some())
    {
        named
    } else if generated.contains_key(reference)
        || docs
            .iter()
            .any(|d| d.get("snapshots").and_then(|v| v.get(reference)).is_some())
    {
        reference.into()
    } else {
        named
    }
}
