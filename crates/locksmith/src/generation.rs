//! Update a previous lockfile from the current workspace declarations.
mod choices;
mod compat;
mod format;
mod graph;
mod identity;
mod incremental;
mod peers;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde_json::json;

use crate::{
    DocumentKind, ExtensionChecksum, Lockfile, LockfileError, PeerRules, PeerVersionMatching,
    RegistryMetadata, RegistryRequest, Value, WorkspaceConfig, inspect_peers, manifest, reconcile,
};

type Result<T> = std::result::Result<T, LockfileError>;

/// A manifest and the exact source identity to record in the package graph.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SourceManifest {
    pub key: String,
    pub manifest: Value,
    pub resolution: Value,
}

impl SourceManifest {
    /// A source that is not a registry publication, such as a Git commit, a
    /// tarball or a local directory. `key` is its package key in the lockfile.
    pub fn new(key: impl Into<String>, manifest: Value, resolution: Value) -> Result<Self> {
        let key = key.into();
        if manifest::selector(&key).1.is_none_or(str::is_empty) {
            return Err(reconcile::missing(
                &[&key],
                "expected a name@locator package key",
            ));
        }
        if !manifest.is_object() {
            return Err(LockfileError::InvalidManifest(key));
        }
        if !resolution.is_object() {
            return Err(reconcile::missing(&[&key], "expected a resolution mapping"));
        }
        let release = Self {
            key,
            manifest,
            resolution,
        };
        release.validate_directory()?;
        Ok(release)
    }

    fn validate_directory(&self) -> Result<()> {
        if self.resolution["type"] == "directory" || self.resolution.get("directory").is_some() {
            let directory = self.resolution["directory"].as_str();
            let locator = manifest::selector(&self.key)
                .1
                .and_then(|locator| locator.strip_prefix("file:"));
            if directory.is_none() || directory != locator {
                return Err(reconcile::missing(
                    &[&self.key, "resolution", "directory"],
                    "directory must match the file: package locator",
                ));
            }
        }
        Ok(())
    }

    /// Check the registry identity and distribution integrity before using the release.
    pub fn registry(
        name: &str,
        version: &str,
        raw: Value,
        registry: &str,
        include_tarball: bool,
    ) -> Result<Self> {
        let mut metadata = RegistryMetadata::default();
        metadata.insert(
            &RegistryRequest::Manifest {
                name: name.into(),
                version: version.into(),
            },
            raw,
            registry,
            include_tarball,
        )?;
        let (key, publication) = metadata.releases.pop_first().expect("one publication");
        Ok(Self {
            key,
            manifest: publication.manifest,
            resolution: publication.resolution,
        })
    }
}

/// A registry package and, when the workspace declares one, its lockfile prefix.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[non_exhaustive]
pub struct RegistryPackage {
    pub name: String,
    pub prefix: Option<String>,
}
impl RegistryPackage {
    pub fn key(&self, version: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{}@{prefix}:{version}", self.name),
            None => format!("{}@{version}", self.name),
        }
    }
    pub fn selector(name: &str, specifier: &str) -> Option<(Self, String)> {
        let specifier = if specifier.trim().is_empty() {
            "*"
        } else {
            specifier
        };
        let RegistryRequest::Version { name, selector } =
            RegistryRequest::version(name, specifier)?
        else {
            return None;
        };
        Some((Self { name, prefix: None }, selector))
    }
    fn from_key(key: &str) -> Option<(Self, &str)> {
        let (name, locator) = manifest::selector(crate::graph::package_key(key));
        let (prefix, version) = locator?
            .split_once(':')
            .map_or((None, locator?), |(p, v)| (Some(p), v));
        if prefix == Some("runtime") && matches!(name, "node" | "bun" | "deno") {
            return None;
        }
        version.parse::<node_semver::Version>().ok()?;
        Some((
            Self {
                name: name.into(),
                prefix: prefix.map(str::to_owned),
            },
            version,
        ))
    }
}

/// The caller supplies version candidates in policy preference order and reads manifests
/// on demand. Generation never invokes a package manager or accesses the filesystem itself.
pub trait PackageSource {
    /// Load the exact manifests that the current dependency frontier requires.
    fn prepare_releases(&mut self, _releases: &BTreeSet<(RegistryPackage, String)>) -> Result<()> {
        Ok(())
    }
    /// Prepare evidence for incumbent releases when the source enforces a
    /// publication policy. Only lookups used by the solver can reject a graph.
    fn prepare_pins(&mut self, _packages: &BTreeSet<RegistryPackage>) -> Result<()> {
        Ok(())
    }
    fn registry_selector(
        &mut self,
        name: &str,
        specifier: &str,
    ) -> Result<Option<(RegistryPackage, String)>> {
        Ok(RegistryPackage::selector(name, specifier))
    }
    fn versions(
        &mut self,
        package: &RegistryPackage,
        selector: &str,
        direct: bool,
    ) -> Result<Vec<String>>;
    fn release(&mut self, package: &RegistryPackage, version: &str) -> Result<SourceManifest>;
    /// Recheck an incumbent when the caller has an active publication policy.
    fn accepts_pin(
        &mut self,
        _package: &RegistryPackage,
        _version: &str,
        _direct: bool,
    ) -> Result<bool> {
        Ok(true)
    }

    /// For time-based resolution, set the transitive cutoff from the selected
    /// direct releases and return their publication times by canonical package key.
    fn direct_times(
        &mut self,
        _direct: &BTreeSet<(RegistryPackage, String)>,
        _recorded: &Value,
    ) -> Result<Option<BTreeMap<String, String>>> {
        Ok(None)
    }

    fn other(&mut self, from: &str, name: &str, specifier: &str) -> Result<SourceManifest> {
        Err(reconcile::missing(
            &[from, name],
            format!("source manifest and resolution required for {specifier}"),
        ))
    }

    /// Read the original manifest of a pinned Git or HTTP archive dependency.
    /// A source can use the locked resolution to find an integrity-checked cache entry.
    fn locked_source(
        &mut self,
        name: &str,
        locator: &str,
        _resolution: &Value,
    ) -> Result<SourceManifest> {
        self.other(".", name, locator)
    }
}

/// Results of evaluating the workspace's pnpmfile in the caller's host.
/// Values exchanged with the core are manifests and the serialized project lock.
pub trait GenerationHooks {
    /// Whether the hook can change package declarations used by the resolver.
    fn reads_packages(&self) -> bool {
        true
    }

    /// Return a replacement for the wanted project lock if a pre-resolution
    /// hook changed its input. A missing replacement preserves the original.
    fn pre_resolution(
        &mut self,
        _previous: &Value,
        _input: &GenerateContext<'_>,
    ) -> Result<Option<Value>> {
        Ok(None)
    }
    fn read_package(&mut self, locator: &str, manifest: Value) -> Result<Value>;
    /// Evaluate a dependency publication. External hosts can return the fields
    /// used for resolution after their hooks finish inspecting the full manifest.
    fn read_dependency(&mut self, locator: &str, manifest: Value) -> Result<Value> {
        self.read_package(locator, manifest)
    }
    fn after_all_resolved(&mut self, document: Value) -> Result<Value> {
        Ok(document)
    }
}

/// The workspace inputs of one generation. Construct with [`GenerateContext::new`].
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct GenerateContext<'a> {
    pub manifests: &'a BTreeMap<String, Value>,
    pub configuration: &'a Value,
    pub pnpm_version: Option<&'a node_semver::Version>,
    pub patches: &'a BTreeMap<String, String>,
}

static NO_PATCHES: BTreeMap<String, String> = BTreeMap::new();

impl<'a> GenerateContext<'a> {
    /// Workspace manifests by importer id and the effective pnpm settings,
    /// with no target pnpm version and no patches.
    pub fn new(manifests: &'a BTreeMap<String, Value>, configuration: &'a Value) -> Self {
        Self {
            manifests,
            configuration,
            pnpm_version: None,
            patches: &NO_PATCHES,
        }
    }
    /// The pnpm release whose lockfile shape and resolver rules to follow.
    pub fn with_pnpm_version(self, version: &'a node_semver::Version) -> Self {
        Self {
            pnpm_version: Some(version),
            ..self
        }
    }
    /// Patch file contents by the path that `patchedDependencies` names.
    pub fn with_patches(self, patches: &'a BTreeMap<String, String>) -> Self {
        Self { patches, ..self }
    }
}

fn header(config: &WorkspaceConfig, input: &GenerateContext<'_>) -> Result<Value> {
    let mut doc = json!({"lockfileVersion":"9.0"});
    reconcile::metadata::settings(&mut doc, config)?;
    if config.resolution.inject_workspace_packages {
        doc["settings"]["injectWorkspacePackages"] = true.into();
    }
    if input.configuration["addMissingPeerTypes"] == true {
        doc["settings"]["addMissingPeerTypes"] = true.into();
    }
    if input.configuration["lockfile"]["includeResolutionSettings"] == true {
        for (name, default) in [
            ("autoDedupe", false),
            ("dedupeInjectedDeps", true),
            ("dedupePeerDependents", true),
            ("linkWorkspacePackages", false),
        ] {
            doc["settings"][name] = input
                .configuration
                .get(name)
                .cloned()
                .unwrap_or_else(|| default.into());
        }
    }
    if let Some(max) = input.configuration.get("peersSuffixMaxLength") {
        let max = max.as_u64().ok_or_else(|| {
            reconcile::missing(&["peersSuffixMaxLength"], "expected a nonnegative integer")
        })?;
        if max != 1000 {
            doc["settings"]["peersSuffixMaxLength"] = max.into();
        }
    }
    if let Some(rules) = reconcile::metadata::effective_overrides(config, input.manifests.get("."))?
    {
        doc["overrides"] = rules;
    }
    reconcile::metadata::patches(&mut doc, config, input.patches)?;
    if let Some(extensions) = config
        .resolution
        .package_extensions
        .as_ref()
        .filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
    {
        let encoding = config.extension_checksum.ok_or_else(|| {
            reconcile::missing(
                &["packageExtensions"],
                "select a pnpm version for the extension checksum",
            )
        })?;
        doc["packageExtensionsChecksum"] =
            crate::checksum::calculate(extensions, encoding == ExtensionChecksum::Md5).into();
    }
    if let Some(ignored) = &config.resolution.ignored_optional_dependencies {
        doc["ignoredOptionalDependencies"] = ignored.clone();
    }
    if let Some(checksum) = &config.resolution.pnpmfile_checksum {
        let digest = checksum
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                let major = input.pnpm_version?.major();
                crate::checksum::hook_digest(checksum, major == 9, major >= 12)
            })
            .ok_or_else(|| {
                reconcile::missing(&["pnpmfile"], "select a pnpm version for the hook checksum")
            })?;
        doc["pnpmfileChecksum"] = digest.into();
    }
    Ok(doc)
}

/// Update a project lockfile from the current manifests. The previous lockfile
/// supplies pinned edges, peer bindings and source blocks, including offline.
pub fn generate(
    previous: &Lockfile,
    input: &GenerateContext<'_>,
    source: &mut impl PackageSource,
) -> Result<Lockfile> {
    generate_inner(previous, input, source, None)
}

/// Generate from the effective configuration, re-evaluating workspace and
/// dependency manifests, then validating the caller's final lock transform.
pub fn generate_with_hooks(
    previous: &Lockfile,
    input: &GenerateContext<'_>,
    source: &mut impl PackageSource,
    hooks: &mut impl GenerationHooks,
) -> Result<Lockfile> {
    generate_inner(previous, input, source, Some(hooks))
}

fn generate_inner<'s>(
    previous: &Lockfile,
    input: &GenerateContext<'_>,
    source: &'s mut impl PackageSource,
    mut hooks: Option<&'s mut dyn GenerationHooks>,
) -> Result<Lockfile> {
    let original = previous
        .project()
        .ok_or_else(|| LockfileError::InvalidDocument("missing project document".into()))?;
    let pre_resolved = match hooks.as_mut() {
        Some(host) => host.pre_resolution(original, input)?,
        None => None,
    };
    if let Some(v) = input.pnpm_version
        && !(9..=12).contains(&v.major())
    {
        return Err(reconcile::missing(
            &["packageManager"],
            format!("expected pnpm 9 through 12, got {v}"),
        ));
    }
    let config = match input.pnpm_version {
        Some(v) => WorkspaceConfig::from_pnpm_version(input.configuration, v)?,
        None => WorkspaceConfig::from_pnpm(input.configuration)?,
    };
    let extensions = compat::Extensions::new(
        &config,
        crate::config::decode(input.configuration, "ignoreCompatibilityDb")?.unwrap_or(false),
    );
    let mut evaluated = std::borrow::Cow::Borrowed(input.manifests);
    if let Some(host) = hooks.as_mut() {
        evaluated = std::borrow::Cow::Owned(
            input
                .manifests
                .iter()
                .map(|(id, raw)| {
                    Ok((
                        id.clone(),
                        host.read_package(id, extensions.manifest(raw, true)?.into_owned())?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        );
    } else {
        for (id, raw) in input.manifests {
            if let std::borrow::Cow::Owned(changed) = extensions.manifest(raw, true)? {
                evaluated.to_mut().insert(id.clone(), changed);
            }
        }
    }
    let manifests = evaluated.as_ref();
    let input = &GenerateContext {
        manifests,
        ..*input
    };
    let root = input
        .manifests
        .get(".")
        .ok_or_else(|| LockfileError::InvalidManifest(".".into()))?;
    for (id, manifest) in input.manifests {
        if !manifest.is_object() {
            return Err(LockfileError::InvalidManifest(id.clone()));
        }
    }
    if let Some(checksum) = &config.resolution.pnpmfile_checksum
        && hooks.is_none()
    {
        let files = checksum
            .as_object()
            .map(|v| v.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "pnpmfile".into());
        return Err(reconcile::missing(
            &["pnpmfile"],
            format!(
                "evaluated hook output required for {files} and its {} workspace project(s)",
                manifests.len()
            ),
        ));
    }
    let base = pre_resolved.as_ref().unwrap_or(original);
    let mut cache = graph::Sources::new(source, hooks, extensions);
    let rules = PeerRules::from_pnpm(
        input.configuration,
        PeerVersionMatching::for_pnpm(input.pnpm_version),
    )?;
    let mut document = solve(
        &mut cache,
        input.manifests,
        &config,
        &header(&config, input)?,
        base,
        Search {
            rules: &rules,
            strict: crate::config::decode(input.configuration, "strictPeerDependencies")?
                .unwrap_or(false),
        },
    )?;
    format::mark_git_archives(&mut document, input.pnpm_version);
    let document = cache.finish_project(document)?;
    let mut documents = BTreeMap::from([(DocumentKind::Project, Arc::new(document))]);
    if let Some(env) = environment(previous, input, root, &mut cache)? {
        documents.insert(DocumentKind::Environment, Arc::new(env));
    }
    let mut result = Lockfile::with_documents(documents, &[previous]);
    result.output_order = crate::source::OutputOrder::Generated {
        pnpm9: input.pnpm_version.is_some_and(|v| v.major() == 9),
    };
    result.validate_with_config(Some(&config))?;
    Ok(result)
}

fn environment(
    previous: &Lockfile,
    input: &GenerateContext<'_>,
    root: &Value,
    cache: &mut graph::Sources<'_>,
) -> Result<Option<Value>> {
    let old = previous.environment().unwrap_or(&Value::Null);
    let major = input.pnpm_version.map_or(0, |v| v.major());
    let manager = root["packageManager"]
        .as_str()
        .and_then(|s| s.strip_prefix("pnpm@"))
        .map(|s| s.split_once("+sha").map_or(s, |(v, _)| v));
    let mut groups = json!({});
    if major >= 11 {
        if let Some(v) = manager
            && (major >= 12
                || old["importers"]["."]
                    .get("packageManagerDependencies")
                    .is_some())
            && input.configuration["pmOnFail"] != "ignore"
        {
            groups["packageManagerDependencies"]["pnpm"] = v.into();
        }
        if let Some(deps) = input.configuration.get("configDependencies") {
            groups["configDependencies"] = deps.clone();
        }
    }
    if groups.as_object().expect("mapping").is_empty() {
        return Ok(None);
    }
    let mut root = json!({"dependencies":{}});
    let mut integrity = BTreeMap::new();
    for group in ["configDependencies", "packageManagerDependencies"] {
        for (name, value) in crate::graph::section(&groups, group).into_iter().flatten() {
            let spec = value
                .as_str()
                .ok_or_else(|| reconcile::missing(&[group, name], "expected a package selector"))?;
            let (version, hash) = spec
                .split_once('+')
                .map_or((spec, None), |(v, h)| (v, Some(h)));
            root["dependencies"][name] = version.into();
            if let Some(hash) = hash {
                integrity.insert(name.clone(), hash.to_owned());
            }
        }
    }
    let mut baseline = old.clone();
    let mut deps = json!({});
    for group in ["configDependencies", "packageManagerDependencies"] {
        for (name, dep) in crate::graph::section(&old["importers"]["."], group)
            .into_iter()
            .flatten()
        {
            deps[name] = dep.clone();
        }
    }
    baseline["importers"]["."] = json!({"dependencies":deps});
    let manifests = BTreeMap::from([(".".into(), root)]);
    let cfg = WorkspaceConfig::default();
    let mut env = solve(
        cache,
        &manifests,
        &cfg,
        &json!({"lockfileVersion":"9.0"}),
        &baseline,
        Search {
            rules: &PeerRules::default(),
            strict: false,
        },
    )?;
    let deps = std::mem::take(&mut env["importers"]["."]["dependencies"]);
    let mut importer = json!({"configDependencies":{}});
    for group in ["configDependencies", "packageManagerDependencies"] {
        for (name, spec) in crate::graph::section(&groups, group).into_iter().flatten() {
            let mut entry = deps[name].clone();
            if let Some(hash) = integrity.get(name) {
                let key = crate::graph::snapshot_key(
                    name,
                    entry["version"].as_str().expect("resolved"),
                    crate::graph::section(&env, "snapshots"),
                );
                if env["packages"][crate::graph::package_key(&key)]["resolution"]["integrity"]
                    != hash.as_str()
                {
                    return Err(reconcile::missing(
                        &[group, name],
                        "config dependency integrity differs from the declaration",
                    ));
                }
            }
            entry["specifier"] = spec.clone();
            importer[group][name] = entry;
        }
    }
    env["importers"]["."] = importer;
    Ok(Some(env))
}

struct Search<'a> {
    rules: &'a PeerRules,
    strict: bool,
}

// pnpm persists publication dates only for the direct references it can look
// up in the finished importer rows. In particular pnpm 9 uses a different
// lookup key for a scoped npm alias than for a regular registry dependency.
fn project_times(doc: &mut Value, old: &Value, config: &WorkspaceConfig) {
    if let Some(times) = reconcile::metadata::publication_times(doc, [doc, old], Some(config)) {
        doc["time"] = times;
    }
}

fn solve(
    cache: &mut graph::Sources<'_>,
    manifests: &BTreeMap<String, Value>,
    config: &WorkspaceConfig,
    header: &Value,
    old: &Value,
    Search { rules, strict }: Search<'_>,
) -> Result<Value> {
    // Peer suffixes rename every instance above a changed provider. An issue
    // is new only if the same release did not already report the same peer
    // requirement against the same provider release.
    let signature = |issue: &crate::PeerIssue| {
        (
            crate::graph::package_key(&issue.snapshot).to_owned(),
            issue.peer.clone(),
            issue.wanted.clone(),
            issue
                .resolved
                .as_deref()
                .map(|v| crate::graph::package_key(v).to_owned()),
        )
    };
    let mut old_issues = None;
    let baseline_issues = || {
        if strict {
            BTreeSet::new()
        } else {
            let lock = Lockfile::with_documents(
                BTreeMap::from([(DocumentKind::Project, Arc::new(old.clone()))]),
                &[],
            );
            inspect_peers(&lock, rules, Some(manifests))
                .iter()
                .map(signature)
                .collect()
        }
    };
    let impact = choices::Impact::new(old);
    let base = graph::Baseline::new(manifests, config, header, old, cache)?;
    base.prepare_importer_pins(cache)?;
    let mut decisions = choices::Agenda::default();
    decisions.candidate(choices::Choices::default());
    let mut visited = BTreeSet::new();
    let mut best_trace: Option<choices::Trace> = None;
    let mut best: Option<(choices::Score, Value)> = None;
    let mut failure = None;
    'search: while let Some(pins) = decisions.next() {
        if !visited.insert(pins.clone()) {
            continue;
        }
        if let Some(trace) = &best_trace
            && trace.matches(&pins)
        {
            decisions.complete_roots(pins, &trace.demands);
            continue;
        }
        let mut build = graph::Build::new(cache, &base, &pins);
        build.prefer_historical_peers = !strict;
        let mut draft;
        let attempt = loop {
            draft = match build.resolve() {
                Ok(draft) => draft,
                Err(e) => {
                    failure = Some(e);
                    continue 'search;
                }
            };
            let retained = if strict {
                None
            } else {
                build.retain_graph(&mut draft)?
            };
            let retained = if retained.is_none() {
                match build.expand(&mut draft) {
                    Ok(()) => None,
                    Err(e) => break Err(e),
                }
            } else {
                retained
            };
            let outcome = match retained {
                Some(doc) if pins.is_empty() => return Ok(doc),
                Some(doc) => Ok(doc),
                None => peers::project(&mut draft, &mut build),
            };
            if outcome.is_ok() || !build.reopen_conflicts() {
                break outcome;
            }
        };
        let mut doc = match attempt {
            Ok(doc) => doc,
            Err(e) => {
                failure = Some(e);
                decisions.equalities(&pins, build.equalities);
                if let Some(parent) = build.blocked_parent.take() {
                    let affected = choices::ancestors(&draft, &BTreeSet::from([parent]));
                    let demands = build.demands;
                    let branches = alternatives(cache, demands.iter(), &pins, &affected, Err)?;
                    decisions.alternatives(&pins, branches);
                }
                continue;
            }
        };
        let demands = build.demands;
        project_times(&mut doc, old, config);
        // A successful incremental graph is final unless this edit added a
        // duplicate release. Preserve the feasibility search for graph errors
        // and strict peers, where a different release can be required.
        let duplicates = (!strict).then(|| choices::introduced_duplicates(&doc, old));
        if best.is_none() && duplicates.as_ref().is_some_and(BTreeSet::is_empty) {
            return Ok(doc);
        }
        let lock = Lockfile::with_documents(
            BTreeMap::from([(DocumentKind::Project, Arc::new(doc))]),
            &[],
        );
        lock.validate_with_config(Some(config))?;
        let issues: Vec<_> = inspect_peers(&lock, rules, Some(manifests))
            .into_iter()
            .filter(|issue| {
                !old_issues
                    .get_or_insert_with(baseline_issues)
                    .contains(&signature(issue))
            })
            .collect();
        let doc = Arc::unwrap_or_clone(lock.documents.into_values().next().expect("one document"));
        let feasible = !strict || issues.is_empty();
        let score = choices::score(&doc, old, &issues, &impact);
        let best_so_far = feasible && best.as_ref().is_none_or(|(s, _)| &score < s);
        if feasible && !best_so_far {
            decisions.complete_roots(pins, &demands);
            continue;
        }
        let lower = choices::lower_bound(&doc, old, &demands);
        if best_so_far && score == lower {
            return Ok(doc);
        }
        if !feasible && best.is_some() {
            continue;
        }
        if best_so_far {
            decisions.clear();
        }
        let upstream = if duplicates.as_ref().is_some_and(|keys| !keys.is_empty()) {
            match choices::reuse_children(&doc, old, &draft, &demands, cache, config, &pins) {
                Ok(proposals) => proposals,
                Err(e) => {
                    failure = Some(e);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let preferred = if duplicates.as_ref().is_some_and(BTreeSet::is_empty) {
            choices::Choices::new()
        } else {
            choices::cover(&demands, &pins)
        };
        let mut peer_targets = BTreeSet::new();
        for issue in &issues {
            peer_targets.insert(crate::graph::package_key(&issue.snapshot).into());
            if let Some((_, pin)) =
                crate::graph::dependency(&doc["snapshots"][&issue.snapshot], &issue.peer)
            {
                let key = crate::graph::snapshot_key(
                    &issue.peer,
                    pin,
                    crate::graph::section(&doc, "snapshots"),
                );
                peer_targets.insert(crate::graph::package_key(&key).into());
            }
        }
        let peer_affected = if feasible || peer_targets.is_empty() {
            BTreeSet::new()
        } else {
            choices::ancestors(&draft, &peer_targets)
        };
        // A strict peer conflict can require changing an ancestor to replace
        // its incompatible subtree.
        let branches = alternatives(cache, demands.iter().rev(), &pins, &peer_affected, |e| {
            failure = Some(e);
            Ok(())
        })?;
        if best_so_far {
            best = Some((score, doc));
        } else if !feasible && failure.is_none() {
            failure = Some(LockfileError::UnmetPeers(issues));
        }
        decisions.alternatives(&pins, branches);
        for proposal in upstream.into_iter().rev() {
            decisions.candidate(proposal);
        }
        let trace_pins = best_so_far.then(|| pins.clone());
        if !preferred.is_empty() {
            let mut families: BTreeMap<_, choices::Choices> = BTreeMap::new();
            for (edge, release) in &preferred {
                families
                    .entry(&demands[edge].package)
                    .or_default()
                    .insert(edge.clone(), release.clone());
            }
            if families.len() > 1 {
                for family in families.into_values().rev() {
                    let mut next = pins.clone();
                    next.extend(family);
                    decisions.candidate(next);
                }
            }
            let mut next = pins;
            next.extend(preferred);
            decisions.candidate(next);
        }
        if let Some(pins) = trace_pins {
            best_trace = Some(choices::Trace { pins, demands });
        }
    }
    best.map(|(_, doc)| doc).ok_or_else(|| {
        failure.unwrap_or_else(|| {
            reconcile::missing(&["dependencies"], "no feasible version assignment")
        })
    })
}

/// Each undecided range selection on a path to `affected`, with its other
/// eligible releases newest first.
fn alternatives<'d>(
    cache: &mut graph::Sources<'_>,
    demands: impl Iterator<Item = (&'d choices::Edge, &'d std::rc::Rc<choices::Demand>)>,
    pins: &choices::Choices,
    affected: &BTreeSet<String>,
    mut lookup_failed: impl FnMut(LockfileError) -> Result<()>,
) -> Result<Vec<(choices::Edge, Vec<String>)>> {
    let mut branches = Vec::new();
    for (edge, demand) in demands {
        if pins.contains_key(edge)
            || demand.selector.parse::<node_semver::Version>().is_ok()
            || !affected.contains(&demand.package.key(&demand.selected))
        {
            continue;
        }
        let direct = matches!(edge.0, choices::Owner::Importer(_));
        match cache.versions(&demand.package, &demand.selector, direct) {
            Ok(all) => {
                let others: Vec<_> = all
                    .iter()
                    .rev()
                    .filter(|v| **v != demand.selected)
                    .cloned()
                    .collect();
                if !others.is_empty() {
                    branches.push((edge.clone(), others));
                }
            }
            Err(e) => lookup_failed(e)?,
        }
    }
    Ok(branches)
}
