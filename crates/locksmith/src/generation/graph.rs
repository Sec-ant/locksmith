use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use serde_json::json;

use super::{
    GenerationHooks, PackageSource, RegistryPackage, Result, SourceManifest,
    choices::{Choices, Demand, Demands, Edge, Owner},
};
use crate::{
    LockfileError, RegistryRequest, Value, WorkspaceConfig, graph, manifest, reconcile,
    workspace_links::{self, WorkspaceLinks},
};

mod history;

fn inject_workspace_dependency(
    name: &str,
    manifest: &Value,
    specifier: &str,
    config: &WorkspaceConfig,
) -> bool {
    !specifier.starts_with("link:") && crate::workspace_links::injected(name, manifest, config)
}

struct Publication {
    source: Rc<SourceManifest>,
    declarations: Rc<reconcile::published::PublishedDeclarations>,
}

pub(super) struct Sources<'a> {
    source: &'a mut dyn PackageSource,
    hooks: Option<&'a mut dyn GenerationHooks>,
    extensions: super::compat::Extensions,
    versions: BTreeMap<(RegistryPackage, String, bool), Vec<String>>,
    selectors: BTreeMap<(String, String), Option<(RegistryPackage, String)>>,
    accepted: [BTreeMap<RegistryPackage, BTreeMap<String, bool>>; 2],
    releases: BTreeMap<String, Rc<SourceManifest>>,
    published_sha1: BTreeMap<String, String>,
    other: BTreeMap<(String, String, String), String>,
    times: Option<BTreeMap<String, String>>,
    policy_epoch: u64,
}
impl<'a> Sources<'a> {
    pub fn new(
        source: &'a mut dyn PackageSource,
        hooks: Option<&'a mut dyn GenerationHooks>,
        extensions: super::compat::Extensions,
    ) -> Self {
        Self {
            source,
            hooks,
            extensions,
            versions: BTreeMap::new(),
            selectors: BTreeMap::new(),
            accepted: Default::default(),
            releases: BTreeMap::new(),
            published_sha1: BTreeMap::new(),
            other: BTreeMap::new(),
            times: None,
            policy_epoch: 0,
        }
    }
    fn reads_packages(&self) -> bool {
        self.hooks.as_ref().is_some_and(|h| h.reads_packages())
    }
    fn insert_release(&mut self, mut release: SourceManifest) -> Result<()> {
        release.validate_directory()?;
        if let Some(hash) = release.manifest["dist"]["shasum"]
            .as_str()
            .and_then(crate::registry::sha1_integrity)
        {
            self.published_sha1.insert(release.key.clone(), hash);
        }
        if let std::borrow::Cow::Owned(extended) =
            self.extensions.manifest(&release.manifest, false)?
        {
            release.manifest = extended;
        }
        if let Some(hooks) = &mut self.hooks {
            release.manifest = hooks.read_dependency(&release.key, release.manifest)?;
            if !release.manifest.is_object() {
                return Err(LockfileError::InvalidManifest(release.key));
            }
        }
        self.releases.insert(release.key.clone(), Rc::new(release));
        Ok(())
    }

    pub fn finish_project(&mut self, document: Value) -> Result<Value> {
        // Environment dependencies use their own configuration and hook context.
        self.extensions = Default::default();
        self.releases.clear();
        self.published_sha1.clear();
        self.other.clear();
        match self.hooks.take() {
            Some(hooks) => hooks.after_all_resolved(document),
            None => Ok(document),
        }
    }
    pub fn versions(
        &mut self,
        package: &RegistryPackage,
        selector: &str,
        direct: bool,
    ) -> Result<&[String]> {
        let key = (package.clone(), selector.to_owned(), direct);
        Ok(match self.versions.entry(key) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                // A custom source may repeat a release. Keep its first position.
                let mut seen = BTreeSet::new();
                let mut versions = self.source.versions(package, selector, direct)?;
                versions.retain(|v| seen.insert(v.clone()));
                entry.insert(versions)
            }
        })
    }
    fn registry_selector(
        &mut self,
        name: &str,
        specifier: &str,
    ) -> Result<Option<(RegistryPackage, String)>> {
        let key = (name.to_owned(), specifier.to_owned());
        let selection = match self.selectors.entry(key) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(self.source.registry_selector(name, specifier)?)
            }
        };
        Ok(selection.clone())
    }
    fn accepts_pin(
        &mut self,
        package: &RegistryPackage,
        version: &str,
        direct: bool,
    ) -> Result<bool> {
        let accepted = &mut self.accepted[usize::from(direct)];
        if let Some(eligible) = accepted
            .get(package)
            .and_then(|versions| versions.get(version))
        {
            return Ok(*eligible);
        }
        let eligible = self.source.accepts_pin(package, version, direct)?;
        accepted
            .entry(package.clone())
            .or_default()
            .insert(version.to_owned(), eligible);
        Ok(eligible)
    }
    fn other(&mut self, dir: &str, name: &str, spec: &str) -> Result<String> {
        let req = (dir.to_owned(), name.to_owned(), spec.to_owned());
        if !self.other.contains_key(&req) {
            let release = self.source.other(dir, name, spec)?;
            let key = release.key.clone();
            // Remember the request only once its hook evaluation has succeeded.
            self.insert_release(release)?;
            self.other.insert(req.clone(), key);
        }
        Ok(self.other[&req].clone())
    }
    fn locked_source(&mut self, key: &str, resolution: &Value) -> Result<&Rc<SourceManifest>> {
        let verify = |release: &SourceManifest| -> Result<()> {
            if let Some(hash) = resolution.get("integrity")
                && release.resolution.get("integrity") != Some(hash)
            {
                return Err(reconcile::missing(
                    &[key],
                    "source integrity differs from the locked release",
                ));
            }
            Ok(())
        };
        if let Some(release) = self.releases.get(key) {
            verify(release)?;
        } else {
            let (name, locator) = manifest::selector(key);
            let locator = locator.expect("locked source locator");
            let release = self.source.locked_source(name, locator, resolution)?;
            verify(&release)?;
            self.insert_requested_release(key, release)?;
        }
        Ok(&self.releases[key])
    }

    fn insert_requested_release(&mut self, key: &str, release: SourceManifest) -> Result<()> {
        if release.key != key {
            return Err(reconcile::missing(
                &[key],
                format!("source manifest identifies {}", release.key),
            ));
        }
        self.insert_release(release)
    }

    pub(super) fn release(&mut self, key: &str) -> Result<&Rc<SourceManifest>> {
        if !self.releases.contains_key(key) {
            let (package, version) = RegistryPackage::from_key(key)
                .ok_or_else(|| reconcile::missing(&[key], "source manifest required"))?;
            let release = self.source.release(&package, version)?;
            self.insert_requested_release(key, release)?;
        }
        Ok(&self.releases[key])
    }

    /// Dynamic hooks can require fetching a full manifest and executing user
    /// code for each release. Speculative moves reuse evaluated hook results.
    pub(super) fn speculative_release(&mut self, key: &str) -> Result<Option<&SourceManifest>> {
        if self.reads_packages() && !self.releases.contains_key(key) {
            return Ok(None);
        }
        self.release(key).map(|release| Some(release.as_ref()))
    }
}

#[derive(Clone)]
pub(super) struct Dependency {
    pub group: &'static str,
    pub target: String,
    pub locked: bool,
}
pub(super) type Dependencies = BTreeMap<String, Dependency>;

pub(super) struct Node {
    pub package: String,
    pub record: Value,
    pub edges: Dependencies,
    pub body: Value,
    pub patch: Option<String>,
    pub reused: bool,
}

pub(super) struct Draft {
    pub document: Value,
    pub roots: BTreeMap<String, Dependencies>,
    pub nodes: BTreeMap<String, Rc<Node>>,
}

/// Resolving an ordinary release with its incumbent pins produces the same outgoing
/// edges in every candidate with the same publication cutoff. Peer binding still runs
/// in the consuming workspace after this stage.
struct Expansion {
    node: Rc<Node>,
    children: Vec<String>,
    demands: Vec<(Edge, Rc<Demand>)>,
}
#[derive(Default)]
struct Expansions {
    policy_epoch: u64,
    natural: BTreeMap<String, Expansion>,
}

/// Workspace and incumbent indices are immutable across alternative release assignments.
pub(super) struct Baseline<'a> {
    manifests: &'a BTreeMap<String, Value>,
    declarations: BTreeMap<&'a str, manifest::Declarations<'a>>,
    publications: RefCell<BTreeMap<(String, String), Publication>>,
    config: &'a WorkspaceConfig,
    header: &'a Value,
    old: &'a Value,
    links: WorkspaceLinks<'a>,
    rules: manifest::Overrides<'a>,
    changed_overrides: Option<BTreeSet<&'a str>>,
    same_override_rules: bool,
    // Keep the incumbent instance when consumers of the same release had different children.
    locked: BTreeMap<RegistryPackage, BTreeMap<String, Vec<String>>>,
    matching: std::cell::RefCell<BTreeMap<(RegistryPackage, String), Vec<String>>>,
    expansions: std::cell::RefCell<Expansions>,
    partition: super::incremental::Partition<'a>,
    history_validation: RefCell<Option<history::Validation<'a>>>,
    // Locked releases whose recorded declarations may disagree with the
    // compatibility database under the current setting.
    compat_candidates: BTreeSet<&'a str>,
    // Settings that rewrite published declarations without appearing in their records.
    declaration_inputs_changed: bool,
}

impl<'a> Baseline<'a> {
    pub fn new(
        manifests: &'a BTreeMap<String, Value>,
        config: &'a WorkspaceConfig,
        header: &'a Value,
        old: &'a Value,
        cache: &Sources<'_>,
    ) -> Result<Self> {
        let mut locked = BTreeMap::<RegistryPackage, BTreeMap<String, Vec<String>>>::new();
        for snapshot in graph::section(old, "snapshots")
            .into_iter()
            .flat_map(|s| s.keys())
        {
            if let Some((package, version)) = RegistryPackage::from_key(snapshot) {
                locked
                    .entry(package)
                    .or_default()
                    .entry(version.to_owned())
                    .or_default()
                    .push(snapshot.clone());
            }
        }
        let snapshots = graph::section(old, "snapshots");
        let compat_candidates = graph::section(old, "packages")
            .into_iter()
            .flatten()
            .filter(|(key, record)| {
                RegistryPackage::from_key(key).is_some_and(|(package, version)| {
                    cache.extensions.disagrees(
                        &package.name,
                        version,
                        record,
                        locked
                            .get(&package)
                            .and_then(|releases| releases.get(version))
                            .into_iter()
                            .flatten()
                            .filter(|snapshot| graph::package_key(snapshot) == *key)
                            .filter_map(|snapshot| snapshots?.get(snapshot)),
                    )
                })
            })
            .map(|(key, _)| key.as_str())
            .collect();
        let rules = manifest::Overrides::new(header, config);
        let same_override_rules = manifest::Overrides::new(old, config).same_rules(&rules);
        let declarations = manifests
            .iter()
            .map(|(id, raw)| Ok((id.as_str(), manifest::declarations(raw, &rules, id)?)))
            .collect::<Result<_>>()?;
        Ok(Self {
            manifests,
            declarations,
            publications: Default::default(),
            config,
            header,
            old,
            locked,
            matching: Default::default(),
            expansions: Default::default(),
            rules,
            same_override_rules,
            links: WorkspaceLinks::new(manifests, config),
            changed_overrides: super::incremental::changed_overrides(old, header),
            partition: super::incremental::Partition::new(old, config),
            history_validation: Default::default(),
            compat_candidates,
            declaration_inputs_changed: [
                "packageExtensionsChecksum",
                "ignoredOptionalDependencies",
                "pnpmfileChecksum",
            ]
            .iter()
            .any(|f| old.get(*f) != header.get(*f))
                || old["settings"].get("addMissingPeerTypes")
                    != header["settings"].get("addMissingPeerTypes"),
        })
    }
    /// Prepare publication evidence in one batch for the locked releases that
    /// importers with unchanged declarations still reach.
    pub fn prepare_importer_pins(&self, cache: &mut Sources<'_>) -> Result<()> {
        let snapshots = graph::section(self.old, "snapshots");
        let history = self.partition.history();
        let mut pending = Vec::new();
        for (&id, declarations) in &self.declarations {
            for (&name, (_, spec)) in declarations {
                if let Some(old) = importer_pin(self.old, id, name)
                    && old["specifier"] == spec.as_ref()
                    && let Some(pin) = old["version"].as_str()
                    && let Some(root) = history
                        .snapshots
                        .get_index_of(graph::snapshot_key(name, pin, snapshots).as_str())
                {
                    pending.push(root);
                }
            }
        }
        let mut seen = vec![false; history.snapshots.len()];
        let mut packages = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if std::mem::replace(&mut seen[id], true) {
                continue;
            }
            let node = &history.snapshots[id];
            if let Some((package, _)) = RegistryPackage::from_key(node.package) {
                packages.insert(package);
            }
            pending.extend(&node.children);
        }
        cache.source.prepare_pins(&packages)
    }
    fn publication(
        &self,
        dir: &str,
        release: &Rc<SourceManifest>,
    ) -> Result<Rc<reconcile::published::PublishedDeclarations>> {
        // Overrides may resolve paths relative to the package's directory.
        let key = (release.key.clone(), dir.to_owned());
        let mut publications = self.publications.borrow_mut();
        if let Some(cached) = publications.get(&key)
            && Rc::ptr_eq(&cached.source, release)
        {
            return Ok(Rc::clone(&cached.declarations));
        }
        let declarations = Rc::new(reconcile::published::published_declarations(
            &release.manifest,
            &release.key,
            dir,
            &self.rules,
            self.config,
        )?);
        publications.insert(
            key,
            Publication {
                source: Rc::clone(release),
                declarations: Rc::clone(&declarations),
            },
        );
        Ok(declarations)
    }
    fn matching(&self, package: &RegistryPackage, selector: &str) -> Vec<String> {
        let Some(locked) = self.locked.get(package) else {
            return Vec::new();
        };
        let mut matching = self.matching.borrow_mut();
        matching
            .entry((package.clone(), selector.to_owned()))
            .or_insert_with(|| {
                let range = selector.parse::<node_semver::Range>().ok();
                let mut versions: Vec<_> = locked
                    .keys()
                    .filter_map(|v| {
                        let semver = v.parse::<node_semver::Version>().ok()?;
                        (selector == "*" || range.as_ref().is_some_and(|r| semver.satisfies(r)))
                            .then_some((semver, v))
                    })
                    .collect();
                versions.sort_by(|(a, _), (b, _)| b.cmp(a));
                versions.into_iter().map(|(_, v)| v.clone()).collect()
            })
            .clone()
    }
    /// Whether a release declares the same dependency selector that already
    /// had no eligible target. A release the source cannot load is kept, so
    /// evaluating it reports its own error.
    pub fn repeats_requirement(
        &self,
        cache: &mut Sources<'_>,
        key: &str,
        name: &str,
        spec: &str,
    ) -> Result<bool> {
        if cache.reads_packages() && !cache.releases.contains_key(key) {
            return Ok(false);
        }
        let Ok(release) = cache.release(key).map(Rc::clone) else {
            return Ok(false);
        };
        Ok(self
            .publication(".", &release)?
            .ordinary
            .get(name)
            .is_some_and(|(_, declared)| declared == spec))
    }
}

fn importer_pin<'a>(old: &'a Value, id: &str, name: &str) -> Option<&'a Value> {
    manifest::GROUPS
        .iter()
        .find_map(|group| old["importers"][id][group].get(name))
}

/// A release whose dependency declaration had no eligible target, and that
/// declaration when a selector caused the failure.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Blocked {
    pub parent: String,
    pub requirement: Option<(String, String)>,
}

impl Blocked {
    fn requirement(parent: &str, name: &str, spec: &str) -> Self {
        Self {
            parent: graph::package_key(parent).to_owned(),
            requirement: Some((name.to_owned(), spec.to_owned())),
        }
    }
    pub fn release(key: &str) -> Self {
        Self {
            parent: graph::package_key(key).to_owned(),
            requirement: None,
        }
    }
}

pub(super) struct Relaxation {
    allow_optional_peers: bool,
    project_interiors: bool,
    collapse_collisions: bool,
    reopened: BTreeSet<String>,
}

pub(super) struct Build<'a, 'b, 's> {
    cache: &'a mut Sources<'s>,
    pub manifests: &'b BTreeMap<String, Value>,
    pub config: &'b WorkspaceConfig,
    header: &'b Value,
    pub(super) old: &'b Value,
    base: &'a Baseline<'b>,
    pins: &'a Choices,
    pinned_snapshots: BTreeSet<&'a str>,
    pub demands: Demands,
    pub blocked: Option<Blocked>,
    pub equalities: Vec<(Vec<Edge>, Vec<String>)>,
    pub conflicting_history: BTreeSet<String>,
    reopened: BTreeSet<String>,
    pub used_interior_history: bool,
    pub project_interiors: bool,
    pub prefer_historical_peers: bool,
    pub(super) seeded_history: bool,
    /// pnpm reads every manifest again once the lockfile is out of date.
    rereads_manifests: bool,
    pub allow_optional_peers: bool,
    pub optional_peer_collision: bool,
    /// Distinct bodies share a key and no release choice can reconcile them.
    pub unrepairable_collision: bool,
    /// Keep the first, shallowest body of each key, as pnpm does.
    pub collapse_collisions: bool,
    local_dirs: BTreeMap<String, String>,
    pending: BTreeSet<String>,
}

impl<'a, 'b, 's> Build<'a, 'b, 's> {
    pub fn new(cache: &'a mut Sources<'s>, base: &'a Baseline<'b>, pins: &'a Choices) -> Self {
        Self {
            cache,
            base,
            pinned_snapshots: pins
                .keys()
                .filter_map(|(owner, _)| match owner {
                    Owner::Snapshot(key) => Some(key.as_str()),
                    _ => None,
                })
                .collect(),
            pins,
            manifests: base.manifests,
            config: base.config,
            header: base.header,
            old: base.old,
            local_dirs: BTreeMap::new(),
            pending: BTreeSet::new(),
            demands: BTreeMap::new(),
            blocked: None,
            equalities: Vec::new(),
            conflicting_history: BTreeSet::new(),
            reopened: BTreeSet::new(),
            used_interior_history: false,
            project_interiors: false,
            prefer_historical_peers: false,
            seeded_history: false,
            rereads_manifests: true,
            allow_optional_peers: true,
            optional_peer_collision: false,
            unrepairable_collision: false,
            collapse_collisions: false,
        }
    }
    pub fn resolve(&mut self) -> Result<Draft> {
        let mut draft = Draft {
            document: self.header.clone(),
            roots: BTreeMap::new(),
            nodes: BTreeMap::new(),
        };
        for (id, raw) in self.manifests {
            let mut importer = json!({});
            for (key, value) in [
                ("dependenciesMeta", raw.get("dependenciesMeta")),
                ("publishDirectory", raw["publishConfig"].get("directory")),
                ("linkDirectory", manifest::link_directory(raw, self.config)),
            ] {
                if let Some(v) = value {
                    importer[key] = v.clone();
                }
            }
            let mut deps = Dependencies::new();
            for (&name, &(group, ref spec)) in &self.base.declarations[id.as_str()] {
                let old = importer_pin(self.old, id, name);
                let target = self.select(Owner::Importer(id.clone()), id, raw, name, spec, old)?;
                importer[group][name] = json!({"specifier":spec,"version":""});
                if let Some(catalog) = spec.strip_prefix("catalog:") {
                    let catalog = if catalog.is_empty() {
                        "default"
                    } else {
                        catalog
                    };
                    let declared = &self.config.catalogs[catalog][name];
                    let version = match (target.starts_with("link:"), &self.config.pnpm_version) {
                        (true, Some(v)) if v.major() == 12 => {
                            (v.minor() >= 6).then_some(declared.as_str())
                        }
                        (true, _) => Some(
                            self.base
                                .links
                                .resolve_for_generation(name, spec, id, raw, true)?
                                .and_then(|(_, member)| member?.get("version")?.as_str())
                                .unwrap_or(declared),
                        ),
                        (false, _) => Some(
                            manifest::selector(graph::package_key(&target))
                                .1
                                .expect("source locator"),
                        ),
                    };
                    if let Some(version) = version {
                        draft.document["catalogs"][catalog][name] =
                            json!({"specifier":declared,"version":version});
                    }
                }
                deps.insert(
                    name.into(),
                    Dependency {
                        group,
                        target,
                        locked: false,
                    },
                );
            }
            draft.document["importers"][id] = importer;
            draft.roots.insert(id.clone(), deps);
        }
        let direct = self
            .demands
            .values()
            .map(|d| (d.package.clone(), d.selected.clone()))
            .collect();
        if let Some(times) = self.cache.source.direct_times(&direct, &self.old["time"])? {
            if self.cache.times.as_ref() != Some(&times) {
                self.cache.versions.retain(|(_, _, direct), _| *direct);
                self.cache.accepted[0].clear();
                self.cache.policy_epoch += 1;
                self.cache.times = Some(times.clone());
            }
            let legacy = self
                .config
                .pnpm_version
                .as_ref()
                .is_some_and(|v| v.major() <= 10);
            let times: serde_json::Map<_, _> = times
                .into_iter()
                .map(|(k, v)| (if legacy { format!("/{k}") } else { k }, Value::from(v)))
                .collect();
            draft.document["time"] = times.into();
        }
        Ok(draft)
    }

    /// A fixed historical peer instance and a newly projected consumer can
    /// require incompatible children at the same physical key. Reopen that
    /// instance and its callers before changing any selected release.
    pub fn reopen_conflicts(&mut self) -> bool {
        if self.optional_peer_collision {
            // A hoisted optional peer can make distinct historical instances
            // compete for the same key. Keep the previous scopes for this
            // assignment if that optional binding cannot be represented.
            self.allow_optional_peers = false;
            self.optional_peer_collision = false;
            self.conflicting_history.clear();
        } else if self.used_interior_history && !self.project_interiors {
            // Two newly emitted keys can collide because their parents supplied
            // different historical children, even when neither key was retained.
            self.project_interiors = true;
            self.conflicting_history.clear();
        } else if !self.conflicting_history.is_subset(&self.reopened) {
            self.reopened.append(&mut self.conflicting_history);
        } else if self.unrepairable_collision && !self.collapse_collisions {
            // pnpm keeps the body of the shallowest occurrence of a dependency
            // path, even when deeper occurrences resolved different children.
            self.collapse_collisions = true;
        } else {
            return false;
        }
        self.unrepairable_collision = false;
        self.used_interior_history = false;
        self.pending.clear();
        self.local_dirs.clear();
        self.demands.clear();
        self.equalities.clear();
        self.blocked = None;
        true
    }

    /// The projection fallbacks this assignment needed. Alternative assignments
    /// share most of its graph and start from the same fallbacks.
    pub fn relaxation(&self) -> Relaxation {
        Relaxation {
            allow_optional_peers: self.allow_optional_peers,
            project_interiors: self.project_interiors,
            collapse_collisions: self.collapse_collisions,
            reopened: self.reopened.clone(),
        }
    }

    pub fn relax(&mut self, relaxation: &Relaxation) {
        self.allow_optional_peers = relaxation.allow_optional_peers;
        self.project_interiors = relaxation.project_interiors;
        self.collapse_collisions = relaxation.collapse_collisions;
        self.reopened.clone_from(&relaxation.reopened);
    }

    fn select(
        &mut self,
        owner: Owner,
        dir: &str,
        parent: &Value,
        name: &str,
        spec: &str,
        old_entry: Option<&Value>,
    ) -> Result<String> {
        let direct = matches!(owner, Owner::Importer(_));
        let declared = spec;
        let catalog = spec.strip_prefix("catalog:");
        let spec = match catalog {
            Some(c) => self
                .config
                .catalogs
                .get(if c.is_empty() { "default" } else { c })
                .and_then(|m| m.get(name))
                .map(String::as_str)
                .ok_or_else(|| {
                    reconcile::missing(&[dir, name], format!("catalog {c:?} has no entry"))
                })?,
            None => spec,
        };
        let old_ref = old_entry.and_then(|v| v["version"].as_str().or_else(|| v.as_str()));
        let old_key =
            old_ref.map(|v| graph::snapshot_key(name, v, graph::section(self.old, "snapshots")));
        let old_key = old_key
            .as_deref()
            .filter(|k| self.old["snapshots"].get(*k).is_some());
        let local = self
            .base
            .links
            .resolve_for_generation(name, spec, dir, parent, direct)?;
        let inject =
            local.is_some() && inject_workspace_dependency(name, parent, spec, self.config);
        if let Some((target, member)) = &local {
            let already_local = self.links_to(dir, target, inject, old_ref, old_key);
            if already_local
                || spec.starts_with("workspace:")
                || spec.starts_with("link:")
                || workspace_links::prefers_workspace(self.config)
            {
                return self.workspace_target(target, *member, inject, old_key);
            }
        }
        if spec.starts_with("workspace:") {
            if let Owner::Snapshot(parent) = &owner {
                self.blocked = Some(Blocked::requirement(parent, name, declared));
            }
            return Err(reconcile::missing(
                &[dir, name],
                format!("no workspace member satisfies {spec}"),
            ));
        }
        let Some((package, selector)) = self.cache.registry_selector(name, spec)? else {
            if let Some(path) = spec.strip_prefix("file:") {
                let path = graph::linked_importer(dir, path);
                if let Some(raw) = self.manifests.get(&path) {
                    return self.directory(&path, raw, old_key);
                }
            }
            let same_runtime = old_key
                .and_then(|key| manifest::selector(graph::package_key(key)).1)
                .and_then(|locator| locator.strip_prefix("runtime:"))
                .is_some_and(|version| {
                    spec.strip_prefix("runtime:")
                        .or_else(|| spec.strip_prefix("tool:"))
                        .is_some_and(|range| manifest::matches_version(range, Some(version)))
                });
            let same_locked_locator = old_key
                .and_then(|key| manifest::selector(graph::package_key(key)).1)
                .is_some_and(|locator| spec == locator && !spec.starts_with("file:"));
            let immutable = same_runtime
                || same_locked_locator
                || old_entry.is_some_and(|v| v["specifier"] == spec) && !spec.starts_with("file:");
            let mut key = match old_key.filter(|_| immutable) {
                Some(key) => key.to_owned(),
                None => self.cache.other(dir, name, spec)?,
            };
            if let Some(path) = self
                .cache
                .releases
                .get(&key)
                .and_then(|r| r.resolution["directory"].as_str())
            {
                self.local_dirs.insert(key.clone(), path.to_owned());
                if let Some(previous) = old_key.filter(|old| graph::package_key(old) == key) {
                    key = previous.to_owned();
                }
            }
            self.pending.insert(key.clone());
            return Ok(key);
        };
        let selector = selector.as_str();
        let edge = (owner, name.to_owned());
        let index = self.base.locked.get(&package);
        let mut locked = Vec::new();
        for version in self.base.matching(&package, selector) {
            if self.cache.accepts_pin(&package, &version, direct)? {
                locked.push(version);
            }
        }
        let incumbent = old_key.and_then(|k| {
            let (p, v) = RegistryPackage::from_key(k)?;
            (p == package
                && (locked.iter().any(|s| s == v)
                    || (old_entry.is_some_and(|d| d["specifier"] == spec)
                        && selector.parse::<node_semver::Range>().is_err())))
            .then_some(v)
        });
        if let Some(v) = incumbent
            && self.cache.accepts_pin(&package, v, direct)?
            && !locked.iter().any(|s| s == v)
        {
            locked.push(v.into());
        }
        let incumbent = incumbent.filter(|v| locked.iter().any(|s| s == v));
        let versions = if locked.is_empty() {
            let mut eligible = match self.cache.versions(&package, selector, direct) {
                Ok(versions) => versions.to_vec(),
                // pnpm falls back to a matching workspace package when registry lookup fails.
                Err(error) => {
                    return match local {
                        Some((target, member)) => {
                            self.workspace_target(&target, member, inject, old_key)
                        }
                        None => Err(error),
                    };
                }
            };
            if let Some(v) = incumbent
                && !eligible.iter().any(|x| x == v)
            {
                eligible.push(v.to_owned());
            }
            eligible
        } else {
            locked
        };
        if let Some((target, Some(member))) = &local
            && incumbent
                .or_else(|| versions.first().map(String::as_str))
                .is_none_or(|version| {
                    workspace_links::prefers_workspace_version(self.config, member, version)
                })
        {
            return self.workspace_target(target, Some(member), inject, old_key);
        }
        if versions.is_empty() {
            if let Owner::Snapshot(parent) = &edge.0 {
                self.blocked = Some(Blocked::requirement(parent, name, declared));
            }
            return Err(reconcile::missing(
                &[&package.name],
                format!(
                    "no eligible registry version of {} satisfies {selector}",
                    package.name
                ),
            ));
        }
        let natural = incumbent.unwrap_or(&versions[0]);
        let choice = self
            .pins
            .get(&edge)
            .map_or(natural, String::as_str)
            .to_owned();
        let natural = self
            .demands
            .get(&edge)
            .is_none_or(|previous| previous.natural.as_deref() == Some(natural))
            .then(|| natural.to_owned());
        if !versions.contains(&choice)
            && !self
                .cache
                .versions(&package, selector, direct)?
                .contains(&choice)
        {
            return Err(reconcile::missing(
                &[&package.name],
                format!("{choice} is ineligible for {selector}"),
            ));
        }
        let key = if Some(choice.as_str()) == incumbent {
            old_key.expect("incumbent snapshot").to_owned()
        } else {
            index
                .and_then(|i| i.get(&choice))
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_else(|| package.key(&choice))
        };
        self.demands.insert(
            edge,
            Rc::new(Demand {
                package,
                selector: selector.into(),
                versions,
                selected: choice,
                natural,
                incumbent: incumbent.is_some(),
            }),
        );
        self.pending.insert(key.clone());
        Ok(key)
    }

    /// Whether a recorded pin names the workspace member at `target`, as its
    /// linked directory or as its injected copy.
    fn links_to(
        &self,
        dir: &str,
        target: &str,
        inject: bool,
        pin: Option<&str>,
        key: Option<&str>,
    ) -> bool {
        let path = target.strip_prefix("link:").expect("workspace link");
        if inject {
            key.and_then(|key| manifest::selector(graph::package_key(key)).1)
                .and_then(|locator| locator.strip_prefix("file:"))
                == Some(path)
        } else {
            pin.and_then(|pin| pin.strip_prefix("link:"))
                .is_some_and(|old| workspace_links::member_target(dir, old, self.config) == path)
        }
    }

    /// Whether a published release's recorded child still satisfies its
    /// declaration, under the rules `select` uses to keep an incumbent.
    pub(super) fn keeps_pin(
        &mut self,
        parent: &Value,
        name: &str,
        spec: &str,
        pin: &str,
    ) -> Result<bool> {
        let key = graph::snapshot_key(name, pin, graph::section(self.old, "snapshots"));
        if spec.starts_with("workspace:") {
            let Some((target, _)) = self
                .base
                .links
                .resolve_for_generation(name, spec, ".", parent, false)?
            else {
                return Ok(false);
            };
            let inject = inject_workspace_dependency(name, parent, spec, self.config);
            return Ok(self.links_to(".", &target, inject, Some(pin), Some(&key)));
        }
        if let Some((package, selector)) = self.cache.registry_selector(name, spec)? {
            let Some((_, version)) =
                RegistryPackage::from_key(&key).filter(|(locked, _)| *locked == package)
            else {
                return Ok(false);
            };
            return Ok(match selector.parse::<node_semver::Range>() {
                // The resolver keeps any locked release for `*`, prereleases included.
                _ if selector == "*" => true,
                Ok(range) => version
                    .parse::<node_semver::Version>()
                    .is_ok_and(|v| v.satisfies(&range)),
                // A dist-tag keeps the release the registry currently selects.
                Err(_) => self
                    .cache
                    .versions(&package, &selector, false)?
                    .first()
                    .is_some_and(|tagged| tagged == version),
            });
        }
        let locator = if pin.starts_with("link:") {
            pin
        } else {
            manifest::selector(graph::package_key(&key))
                .1
                .unwrap_or_default()
        };
        Ok(spec == locator
            || ["file:", "link:"].into_iter().any(|protocol| {
                spec.strip_prefix(protocol)
                    .zip(locator.strip_prefix(protocol))
                    .is_some_and(|(path, locked)| {
                        !path.starts_with('/')
                            && path.as_bytes().get(1) != Some(&b':')
                            && graph::linked_importer(".", path) == locked
                    })
            }))
    }

    fn workspace_target(
        &mut self,
        target: &str,
        member: Option<&Value>,
        inject: bool,
        previous: Option<&str>,
    ) -> Result<String> {
        if !inject {
            return Ok(target.to_owned());
        }
        let path = target.strip_prefix("link:").expect("workspace link");
        let raw = member
            .ok_or_else(|| reconcile::missing(&[path], "missing injected workspace manifest"))?;
        self.directory(path, raw, previous)
    }
    fn directory(&mut self, path: &str, raw: &Value, previous: Option<&str>) -> Result<String> {
        let name = raw["name"]
            .as_str()
            .ok_or_else(|| LockfileError::InvalidManifest(path.into()))?;
        let key = format!("{name}@file:{path}");
        self.cache.releases.entry(key.clone()).or_insert_with(|| {
            Rc::new(SourceManifest {
                key: key.clone(),
                manifest: raw.clone(),
                resolution: json!({"type":"directory","directory":path}),
            })
        });
        self.local_dirs.insert(key.clone(), path.to_owned());
        let key = match previous {
            Some(previous) if graph::package_key(previous) == key => previous.to_owned(),
            _ => key,
        };
        self.pending.insert(key.clone());
        Ok(key)
    }
    pub fn auto_peer(
        &mut self,
        importer: &str,
        owner: &str,
        name: &str,
        spec: &str,
    ) -> Result<Option<String>> {
        let original = &self.old["snapshots"][owner];
        // pnpm 9 and 10 report an unsupplied malformed published peer, while
        // an offered provider still participates in the package identity.
        if self
            .config
            .pnpm_version
            .as_ref()
            .is_some_and(|v| v.major() <= 10)
            && !spec.contains(':')
            && !manifest::valid_peer_range(spec)
            && RegistryPackage::selector(name, spec).is_none()
        {
            return Ok(None);
        }
        if spec.starts_with("workspace:")
            && original.is_object()
            && self
                .base
                .links
                .resolve_for_generation(name, spec, importer, &Value::Null, false)?
                .is_none()
        {
            // A published package may retain an unpublished workspace peer.
            // Its old instance is evidence for the concrete provider (or its absence).
            let Some((_, pin)) = graph::dependency(original, name) else {
                return Ok(None);
            };
            if pin.starts_with("link:") {
                return Ok(Some(pin.into()));
            }
            let key = graph::snapshot_key(name, pin, graph::section(self.old, "snapshots"));
            self.pending.insert(key.clone());
            return Ok(Some(key));
        }
        let old = graph::dependency(original, name)
            .map(|(_, pin)| json!({"specifier":spec,"version":pin}));
        self.select(
            Owner::Snapshot(format!("{importer}:{owner}")),
            importer,
            &Value::Null,
            name,
            spec,
            old.as_ref(),
        )
        .map(Some)
    }
    /// Releases of `name` recorded in the previous lockfile, newest first.
    fn historical_releases(&self, name: &str) -> Vec<(node_semver::Version, String)> {
        let package = RegistryPackage {
            name: name.into(),
            prefix: None,
        };
        let mut versions: Vec<_> = self
            .base
            .locked
            .get(&package)
            .into_iter()
            .flat_map(|versions| versions.keys())
            .filter_map(|v| Some((v.parse::<node_semver::Version>().ok()?, v.clone())))
            .collect();
        versions.sort_by(|a, b| b.0.cmp(&a.0));
        versions
    }

    /// An automatic peer may reuse the previous occurrence's binding or an
    /// eligible locked release. Optional peers require a matching lock entry.
    pub fn historical_peer(
        &mut self,
        owner: &str,
        name: &str,
        ranges: &[&str],
        optional: bool,
    ) -> Result<Option<String>> {
        let package = RegistryPackage {
            name: name.to_owned(),
            prefix: None,
        };
        let parsed: Vec<_> = ranges
            .iter()
            .map(|r| r.parse::<node_semver::Range>())
            .collect();
        if parsed.iter().any(std::result::Result::is_err) {
            return Ok(None);
        }
        let observed =
            graph::dependency(&self.old["snapshots"][owner], name).and_then(|(_, pin)| {
                let key = graph::snapshot_key(name, pin, graph::section(self.old, "snapshots"));
                let (pkg, version) = RegistryPackage::from_key(&key)?;
                (pkg == package).then(|| version.to_owned())
            });
        // pnpm 9 and 10 fill a required peer with the newest locked release
        // whatever its range. Later releases and optional peers need a match.
        let any_range = !optional
            && self
                .config
                .pnpm_version
                .as_ref()
                .is_some_and(|v| v.major() <= 10);
        let fits =
            |v: &node_semver::Version| parsed.iter().flatten().all(|range| v.satisfies(range));
        if let Some(version) = observed
            && (any_range
                || version
                    .parse::<node_semver::Version>()
                    .is_ok_and(|v| fits(&v)))
            && self.cache.accepts_pin(&package, &version, false)?
        {
            return Ok(Some(version));
        }
        for (version, raw) in self.historical_releases(name) {
            if (any_range || fits(&version)) && self.cache.accepts_pin(&package, &raw, false)? {
                return Ok(Some(raw));
            }
        }
        Ok(None)
    }

    /// Consumers may request one missing peer with disjoint ranges. Install the
    /// release that satisfies most of them instead of leaving every consumer
    /// without a provider. Releases already in the lockfile win ties.
    pub fn covering_peer(&mut self, name: &str, ranges: &[&str]) -> Result<Option<String>> {
        let package = RegistryPackage {
            name: name.to_owned(),
            prefix: None,
        };
        // A locked release the publication policy now rejects cannot win a tie.
        let mut candidates = Vec::new();
        for (version, raw) in self.historical_releases(name) {
            if self.cache.accepts_pin(&package, &raw, false)? {
                candidates.push((version, raw, true));
            }
        }
        for range in ranges.iter().copied().collect::<BTreeSet<_>>() {
            let Some((package, selector)) = self.cache.registry_selector(name, range)? else {
                continue;
            };
            if let Some(raw) = self.cache.versions(&package, &selector, false)?.first()
                && !candidates.iter().any(|(_, v, _)| v == raw)
                && let Ok(version) = raw.parse::<node_semver::Version>()
            {
                candidates.push((version, raw.clone(), false));
            }
        }
        let parsed: Vec<_> = ranges
            .iter()
            .filter_map(|r| r.parse::<node_semver::Range>().ok())
            .collect();
        Ok(candidates
            .into_iter()
            .max_by(|a, b| {
                let covered =
                    |v: &node_semver::Version| parsed.iter().filter(|r| v.satisfies(r)).count();
                (covered(&a.0), a.2, &a.0).cmp(&(covered(&b.0), b.2, &b.0))
            })
            .map(|(_, raw, _)| raw))
    }

    pub fn expand(&mut self, draft: &mut Draft) -> Result<()> {
        while !self.pending.is_empty() {
            let frontier = std::mem::take(&mut self.pending);
            if self.cache.reads_packages() {
                let releases = frontier
                    .iter()
                    .filter(|key| {
                        !draft.nodes.contains_key(*key)
                            && !self.cache.releases.contains_key(graph::package_key(key))
                    })
                    .filter_map(|key| {
                        RegistryPackage::from_key(key).map(|(pkg, version)| (pkg, version.into()))
                    })
                    .collect();
                self.cache.source.prepare_releases(&releases)?;
            }
            self.expand_frontier(draft, frontier)?;
        }
        Ok(())
    }

    pub fn reads_packages(&self) -> bool {
        self.cache.reads_packages()
    }
    pub fn partition(&self) -> &'a super::incremental::Partition<'b> {
        &self.base.partition
    }

    fn expand_frontier(&mut self, draft: &mut Draft, frontier: BTreeSet<String>) -> Result<()> {
        let mut next = BTreeSet::new();
        let baseline = self.base;
        let mut expansions = baseline.expansions.borrow_mut();
        if expansions.policy_epoch != self.cache.policy_epoch {
            expansions.natural.clear();
            expansions.policy_epoch = self.cache.policy_epoch;
        }
        for key in frontier {
            if draft.nodes.contains_key(&key) {
                continue;
            }
            let base = graph::package_key(&key);
            let natural = !self.pinned_snapshots.contains(key.as_str())
                && !self.local_dirs.contains_key(base);
            if natural && let Some(prior) = expansions.natural.get(&key) {
                self.demands.extend(prior.demands.iter().cloned());
                next.extend(prior.children.iter().cloned());
                draft.nodes.insert(key, Rc::clone(&prior.node));
                continue;
            }
            let mut record = self.old["packages"][base].clone();
            let original = &self.old["snapshots"][&key];
            let mut edges = Dependencies::new();
            let mut body = json!({});
            let (package_name, locator) = manifest::selector(base);
            let local = locator.and_then(|locator| locator.strip_prefix("file:"));
            if let Some(path) = local {
                if !self.cache.releases.contains_key(base) {
                    let key =
                        self.cache
                            .other(".", package_name, locator.expect("file locator"))?;
                    if key != base {
                        return Err(reconcile::missing(
                            &[base],
                            format!("local manifest now identifies {key}"),
                        ));
                    }
                }
                // Another candidate may have loaded the release. Each build
                // still needs the directory its local children resolve from.
                if self.cache.releases[base].resolution["type"] == "directory" {
                    self.local_dirs.insert(base.to_owned(), path.to_owned());
                }
            }
            let needs_expansion = self.cache.reads_packages()
                || original.is_null()
                || !record.is_object()
                || local.is_some()
                || (self.rereads_manifests && self.base.compat_candidates.contains(base));
            let mut can_reuse = !needs_expansion
                && !self.base.declaration_inputs_changed
                && (self.base.same_override_rules
                    || self.base.changed_overrides.as_ref().is_some_and(|changed| {
                        !super::incremental::observes_override(&record, original, changed)
                    }));
            let snapshots = graph::section(self.old, "snapshots");
            if can_reuse {
                let providers = record.get("peerDependencies").and_then(|_| {
                    reconcile::published::records_fallback_peers(self.config)
                        .then(|| crate::peer::explicit_bindings(&key))
                        .flatten()
                });
                for (group, name, value) in graph::snapshot_dependencies(original) {
                    let target = value.as_str().map(|pin| {
                        if pin.starts_with("link:") {
                            pin.to_owned()
                        } else {
                            graph::snapshot_key(name, pin, snapshots)
                        }
                    });
                    if let Some((package, version)) =
                        target.as_deref().and_then(RegistryPackage::from_key)
                        && !self.cache.accepts_pin(&package, version, false)?
                    {
                        edges.clear();
                        can_reuse = false;
                        break;
                    }
                    if record["peerDependencies"].get(name).is_some()
                        && (package_name == name
                            || providers
                                .as_ref()
                                .is_none_or(|p| p.contains_key(name.as_str())))
                    {
                        continue;
                    }
                    let target = target.ok_or_else(|| {
                        reconcile::missing(&[&key, group, name], "expected a resolved reference")
                    })?;
                    edges.insert(
                        name.clone(),
                        Dependency {
                            group,
                            target,
                            locked: false,
                        },
                    );
                }
            }
            if can_reuse {
                self.pending.extend(
                    edges
                        .values()
                        .filter(|edge| !edge.target.starts_with("link:"))
                        .map(|edge| edge.target.clone()),
                );
                body = original.clone();
                let fields = body.as_object_mut().expect("snapshot mapping");
                for field in [
                    "dependencies",
                    "optionalDependencies",
                    "transitivePeerDependencies",
                    "optional",
                ] {
                    fields.remove(field);
                }
            } else {
                let release = Rc::clone(
                    if local.is_none()
                        && locator.is_some()
                        && RegistryPackage::from_key(base).is_none()
                    {
                        self.cache.locked_source(base, &record["resolution"])?
                    } else {
                        self.cache.release(base)?
                    },
                );
                let mut resolution = std::borrow::Cow::Borrowed(&release.resolution);
                if local.is_none()
                    && let Some(hash) = record["resolution"].get("integrity")
                    && release.resolution.get("integrity") != Some(hash)
                {
                    // Older locks can carry the same tarball's SHA-1 even when
                    // the registry now publishes a stronger default digest.
                    if self
                        .cache
                        .published_sha1
                        .get(base)
                        .is_none_or(|sha| Some(sha.as_str()) != hash.as_str())
                    {
                        return Err(reconcile::missing(
                            &["packages", base],
                            "published integrity differs from the locked release",
                        ));
                    }
                    resolution.to_mut()["integrity"] = hash.clone();
                }
                let raw = &release.manifest;
                record = reconcile::published::package_record(raw, &resolution);
                if RegistryRequest::manifest(base).is_none() {
                    for field in ["name", "version"] {
                        if let Some(v) = raw.get(field)
                            && (field != "name" || manifest::selector(base).0 != v)
                        {
                            record[field] = v.clone();
                        }
                    }
                }
                let dir = self
                    .local_dirs
                    .get(base)
                    .cloned()
                    .unwrap_or_else(|| ".".into());
                let declarations = self.base.publication(&dir, &release)?;
                for (name, (spec, optional)) in &declarations.peers {
                    record["peerDependencies"][name] = spec.as_str().into();
                    if *optional {
                        record["peerDependenciesMeta"][name] = json!({"optional":true});
                    }
                }
                for (name, &(group, ref spec)) in &declarations.ordinary {
                    let old = graph::dependency(original, name).map(|(_, v)| json!({"version":v}));
                    let target = self.select(
                        Owner::Snapshot(key.clone()),
                        &dir,
                        raw,
                        name,
                        spec,
                        old.as_ref(),
                    )?;
                    edges.insert(
                        name.to_owned(),
                        Dependency {
                            group,
                            target,
                            locked: false,
                        },
                    );
                }
                let fallback = reconcile::published::records_fallback_peers(self.config);
                // Before pnpm recorded fallback peers, a release's own dependency
                // served a peer unless pnpm first reached the release under a
                // provider. The previous record shows that outcome for a release
                // it already contains. Keep it instead of rebinding the peer.
                if !fallback && let Some(previous) = self.old["packages"].get(base) {
                    let fell_back: Vec<_> = graph::section(&record, "peerDependencies")
                        .into_iter()
                        .flat_map(|peers| peers.keys())
                        .filter(|name| {
                            edges.contains_key(*name)
                                && previous["peerDependencies"].get(*name).is_none()
                        })
                        .cloned()
                        .collect();
                    for name in fell_back {
                        for field in ["peerDependencies", "peerDependenciesMeta"] {
                            if let Some(entries) =
                                record.get_mut(field).and_then(Value::as_object_mut)
                            {
                                entries.remove(&name);
                                if entries.is_empty() {
                                    record
                                        .as_object_mut()
                                        .expect("package record")
                                        .remove(field);
                                }
                            }
                        }
                    }
                }
            }
            if self.header["settings"]["addMissingPeerTypes"] == true {
                let names: Vec<_> = graph::section(&record, "peerDependencies")
                    .into_iter()
                    .flat_map(|p| p.keys())
                    .filter(|n| !n.starts_with("@types/"))
                    .map(|n| format!("@types/{}", n.trim_start_matches('@').replace('/', "__")))
                    .filter(|n| {
                        !edges.contains_key(n)
                            && record["peerDependencies"].get(n).is_none()
                            && record["peerDependenciesMeta"].get(n).is_none()
                    })
                    .collect();
                for name in names {
                    record["peerDependencies"][&name] = "*".into();
                    record["peerDependenciesMeta"][&name] = json!({"optional":true});
                }
            }
            let patch = reconcile::desired_patch(
                base,
                self.header,
                [self.old, &Value::Null, &Value::Null, &Value::Null],
            )
            .ok_or_else(|| {
                reconcile::missing(&["patchedDependencies", base], "competing patch selectors")
            })?
            .map(str::to_owned);
            let node = Rc::new(Node {
                package: base.into(),
                record,
                edges,
                body,
                patch,
                reused: can_reuse,
            });
            let children = std::mem::take(&mut self.pending);
            if natural
                && local.is_none()
                && !children
                    .iter()
                    .any(|target| self.local_dirs.contains_key(graph::package_key(target)))
            {
                let owner = Owner::Snapshot(key.clone());
                let demands = self
                    .demands
                    .range((owner.clone(), String::new())..)
                    .take_while(|((o, _), _)| *o == owner)
                    .map(|(e, d)| (e.clone(), d.clone()))
                    .collect();
                expansions.natural.insert(
                    key.clone(),
                    Expansion {
                        node: Rc::clone(&node),
                        children: children.iter().cloned().collect(),
                        demands,
                    },
                );
            }
            next.extend(children);
            draft.nodes.insert(key, node);
        }
        self.pending = next;
        Ok(())
    }
}
