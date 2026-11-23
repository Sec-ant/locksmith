use std::{
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
    workspace_links::WorkspaceLinks,
};

mod history;

pub(super) struct Sources<'a> {
    source: &'a mut dyn PackageSource,
    hooks: Option<&'a mut dyn GenerationHooks>,
    extensions: super::compat::Extensions,
    versions: BTreeMap<(RegistryPackage, String, bool), Vec<String>>,
    accepted: [BTreeMap<RegistryPackage, BTreeMap<String, bool>>; 2],
    releases: BTreeMap<String, Rc<SourceManifest>>,
    published_sha1: BTreeMap<String, String>,
    other: BTreeMap<(String, String, String), String>,
    times: Option<BTreeMap<String, String>>,
    policy_epoch: u64,
    // Releases whose resolver declarations the readPackage hook rewrote.
    hook_changed: BTreeSet<String>,
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
            accepted: Default::default(),
            releases: BTreeMap::new(),
            published_sha1: BTreeMap::new(),
            other: BTreeMap::new(),
            times: None,
            policy_epoch: 0,
            hook_changed: BTreeSet::new(),
        }
    }
    fn reads_packages(&self) -> bool {
        self.hooks.as_ref().is_some_and(|h| h.reads_packages())
    }
    fn insert_release(&mut self, mut release: SourceManifest) -> Result<()> {
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
            const FIELDS: [&str; 4] = [
                "dependencies",
                "optionalDependencies",
                "peerDependencies",
                "peerDependenciesMeta",
            ];
            let declared = |manifest: &Value, field: &str| {
                manifest
                    .get(field)
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            };
            let before = FIELDS.map(|field| declared(&release.manifest, field));
            release.manifest = hooks.read_dependency(&release.key, release.manifest)?;
            if !release.manifest.is_object() {
                return Err(LockfileError::InvalidManifest(release.key));
            }
            if FIELDS
                .iter()
                .zip(&before)
                .any(|(field, value)| declared(&release.manifest, field) != *value)
            {
                self.hook_changed.insert(release.key.clone());
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
                entry.insert(self.source.versions(package, selector, direct)?)
            }
        })
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
            self.other.insert(req.clone(), release.key.clone());
            self.insert_release(release)?;
        }
        Ok(self.other[&req].clone())
    }
    pub(super) fn release(&mut self, key: &str) -> Result<&Rc<SourceManifest>> {
        if !self.releases.contains_key(key) {
            let (package, version) = RegistryPackage::from_key(key)
                .ok_or_else(|| reconcile::missing(&[key], "source manifest required"))?;
            let release = self.source.release(&package, version)?;
            if release.key != key {
                return Err(reconcile::missing(
                    &[key],
                    "publication has a different source identity",
                ));
            }
            self.insert_release(release)?;
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
    historical_edges_match: std::cell::OnceCell<bool>,
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
    incomplete: std::cell::RefCell<Option<(u64, BTreeSet<String>)>>,
    expansions: std::cell::RefCell<Expansions>,
    partition: super::incremental::Partition<'a>,
    // Locked releases whose declarations disagree with the compatibility database setting.
    compat_stale: BTreeSet<&'a str>,
}

impl<'a> Baseline<'a> {
    pub fn new(
        manifests: &'a BTreeMap<String, Value>,
        config: &'a WorkspaceConfig,
        header: &'a Value,
        old: &'a Value,
        cache: &Sources<'_>,
    ) -> Self {
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
        let compat_stale = graph::section(old, "packages")
            .into_iter()
            .flatten()
            .filter(|(key, record)| {
                RegistryPackage::from_key(key).is_some_and(|(package, version)| {
                    cache.extensions.disagrees(
                        &package.name,
                        version,
                        record,
                        snapshots
                            .into_iter()
                            .flatten()
                            .filter(|(snapshot, _)| graph::package_key(snapshot) == *key)
                            .map(|(_, body)| body),
                    )
                })
            })
            .map(|(key, _)| key.as_str())
            .collect();
        let rules = manifest::Overrides::new(header, config);
        let same_override_rules = manifest::Overrides::new(old, config).same_rules(&rules);
        Self {
            manifests,
            config,
            header,
            old,
            locked,
            matching: Default::default(),
            expansions: Default::default(),
            incomplete: Default::default(),
            rules,
            same_override_rules,
            links: WorkspaceLinks::new(manifests, config),
            changed_overrides: super::incremental::changed_overrides(old, header),
            partition: super::incremental::Partition::new(old, config),
            compat_stale,
        }
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
}

pub(super) struct Build<'a, 's> {
    cache: &'a mut Sources<'s>,
    pub manifests: &'a BTreeMap<String, Value>,
    pub config: &'a WorkspaceConfig,
    header: &'a Value,
    pub(super) old: &'a Value,
    base: &'a Baseline<'a>,
    pins: &'a Choices,
    pinned_snapshots: BTreeSet<&'a str>,
    pub demands: Demands,
    pub blocked_parent: Option<String>,
    pub equalities: Vec<(Vec<Edge>, Vec<String>)>,
    pub conflicting_history: BTreeSet<String>,
    reopened: BTreeSet<String>,
    pub used_interior_history: bool,
    pub project_interiors: bool,
    pub exhaustive: bool,
    pub prefer_historical_peers: bool,
    pub(super) seeded_history: bool,
    /// pnpm reads every manifest again once the lockfile is out of date.
    rereads_manifests: bool,
    pub allow_optional_peers: bool,
    pub optional_peer_collision: bool,
    pub empty_domain: bool,
    local_dirs: BTreeMap<String, String>,
    pending: BTreeSet<String>,
}

impl<'a, 's> Build<'a, 's> {
    pub fn new(cache: &'a mut Sources<'s>, base: &'a Baseline<'a>, pins: &'a Choices) -> Self {
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
            blocked_parent: None,
            equalities: Vec::new(),
            conflicting_history: BTreeSet::new(),
            reopened: BTreeSet::new(),
            used_interior_history: false,
            project_interiors: false,
            exhaustive: false,
            prefer_historical_peers: false,
            seeded_history: false,
            rereads_manifests: true,
            allow_optional_peers: true,
            optional_peer_collision: false,
            empty_domain: false,
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
            for (name, (group, spec)) in manifest::declarations(raw, &self.base.rules, id)? {
                let old = manifest::GROUPS
                    .iter()
                    .find_map(|g| self.old["importers"][id][g].get(name));
                let target = self.select(Owner::Importer(id.clone()), id, raw, name, &spec, old)?;
                importer[group][name] = json!({"specifier":spec,"version":""});
                if let Some(catalog) = spec.strip_prefix("catalog:") {
                    let catalog = if catalog.is_empty() {
                        "default"
                    } else {
                        catalog
                    };
                    let declared = &self.config.catalogs[catalog][name];
                    let version = match (target.starts_with("link:"), &self.config.pnpm_version) {
                        (true, Some(v)) if v.major == 12 => {
                            (v.minor >= 6).then_some(declared.as_str())
                        }
                        (true, _) => Some(
                            self.base
                                .links
                                .resolve_for_generation(name, &spec, id, raw, true)?
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
                .is_some_and(|v| v.major <= 10);
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
        } else {
            if self.conflicting_history.is_subset(&self.reopened) {
                return false;
            }
            self.reopened.append(&mut self.conflicting_history);
        }
        self.used_interior_history = false;
        self.pending.clear();
        self.local_dirs.clear();
        self.demands.clear();
        self.equalities.clear();
        self.blocked_parent = None;
        self.empty_domain = false;
        true
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
        let injected = crate::workspace_links::injected(name, parent, self.config);
        if let Some((target, package)) = self
            .base
            .links
            .resolve_for_generation(name, spec, dir, parent, direct)?
        {
            if !(injected && !spec.starts_with("link:")) {
                return Ok(target);
            }
            let path = target.strip_prefix("link:").expect("workspace link");
            let raw = package.ok_or_else(|| {
                reconcile::missing(&[dir, name], "missing injected workspace manifest")
            })?;
            return self.directory(path, raw, old_key);
        }
        if spec.starts_with("workspace:") {
            self.empty_domain = true;
            if let Owner::Snapshot(parent) = &owner {
                self.blocked_parent = Some(graph::package_key(parent).to_owned());
            }
            return Err(reconcile::missing(
                &[dir, name],
                format!("no workspace member satisfies {spec}"),
            ));
        }
        let Some((package, selector)) = self.cache.source.registry_selector(name, spec)? else {
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
            let immutable = same_runtime
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
        let versions = if locked.is_empty() || self.exhaustive {
            let mut eligible = self.cache.versions(&package, selector, direct)?.to_vec();
            if let Some(v) = incumbent
                && !eligible.iter().any(|x| x == v)
            {
                eligible.push(v.to_owned());
            }
            eligible
        } else {
            locked
        };
        if versions.is_empty() {
            self.empty_domain = true;
            if let Owner::Snapshot(parent) = &edge.0 {
                self.blocked_parent = Some(graph::package_key(parent).to_owned());
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
            && (self.exhaustive
                || !self
                    .cache
                    .versions(&package, selector, direct)?
                    .contains(&choice))
        {
            self.empty_domain = true;
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
            .is_some_and(|v| v.major <= 10)
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
        if let Some(version) = observed
            && (!optional
                || version
                    .parse::<node_semver::Version>()
                    .is_ok_and(|v| parsed.iter().flatten().all(|range| v.satisfies(range))))
            && self.cache.accepts_pin(&package, &version, false)?
        {
            return Ok(Some(version));
        }
        for (version, raw) in self.historical_releases(name) {
            if (!optional
                || parsed
                    .iter()
                    .flatten()
                    .all(|range| version.satisfies(range)))
                && self.cache.accepts_pin(&package, &raw, false)?
            {
                return Ok(Some(raw));
            }
        }
        Ok(None)
    }

    /// Consumers may request one missing peer with disjoint ranges. Install the
    /// release that satisfies most of them instead of leaving every consumer
    /// without a provider. Releases already in the lockfile win ties.
    pub fn covering_peer(&mut self, name: &str, ranges: &[&str]) -> Result<Option<String>> {
        let mut candidates: Vec<_> = self
            .historical_releases(name)
            .into_iter()
            .map(|(version, raw)| (version, raw, true))
            .collect();
        for range in ranges.iter().copied().collect::<BTreeSet<_>>() {
            let Some((package, selector)) = self.cache.source.registry_selector(name, range)?
            else {
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
            if self.cache.reads_packages() || self.exhaustive {
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
    pub fn partition(&self) -> &'a super::incremental::Partition<'a> {
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
            let natural = !self.exhaustive
                && !self.pinned_snapshots.contains(key.as_str())
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
            let locator = manifest::selector(base).1.unwrap_or_default();
            let local = locator.strip_prefix("file:");
            if let Some(path) = local
                && !self.cache.releases.contains_key(base)
            {
                let key = self.cache.other(".", manifest::selector(base).0, locator)?;
                if key != base {
                    return Err(reconcile::missing(
                        &[base],
                        format!("local manifest now identifies {key}"),
                    ));
                }
                if self.cache.releases[base].resolution["type"] == "directory" {
                    self.local_dirs.insert(base.to_owned(), path.to_owned());
                }
            }
            let can_reuse = !self.exhaustive
                && !self.cache.reads_packages()
                && !original.is_null()
                && record.is_object()
                && local.is_none()
                && !(self.rereads_manifests && self.base.compat_stale.contains(base))
                && (self.base.same_override_rules
                    || self.base.changed_overrides.as_ref().is_some_and(|changed| {
                        !super::incremental::observes_override(&record, original, changed)
                    }))
                && [
                    "packageExtensionsChecksum",
                    "ignoredOptionalDependencies",
                    "pnpmfileChecksum",
                ]
                .iter()
                .all(|f| self.old.get(*f) == self.header.get(*f));
            let mut can_reuse = can_reuse;
            can_reuse &= self.old["settings"].get("addMissingPeerTypes")
                == self.header["settings"].get("addMissingPeerTypes");
            if can_reuse {
                for group in ["dependencies", "optionalDependencies"] {
                    for (name, pin) in graph::section(original, group).into_iter().flatten() {
                        if let Some(pin) = pin.as_str()
                            && let Some((package, version)) =
                                RegistryPackage::from_key(&graph::snapshot_key(
                                    name,
                                    pin,
                                    graph::section(self.old, "snapshots"),
                                ))
                            && !self.cache.accepts_pin(&package, version, false)?
                        {
                            can_reuse = false;
                            break;
                        }
                    }
                    if !can_reuse {
                        break;
                    }
                }
            }
            if can_reuse {
                body = original.clone();
                for f in [
                    "dependencies",
                    "optionalDependencies",
                    "transitivePeerDependencies",
                    "optional",
                ] {
                    body.as_object_mut().expect("snapshot mapping").remove(f);
                }
                for group in ["dependencies", "optionalDependencies"] {
                    for (name, v) in graph::section(original, group).into_iter().flatten() {
                        if record["peerDependencies"].get(name).is_some() {
                            let supplied_by_package =
                                super::peers::records_fallback_peers(self.config)
                                    && manifest::selector(base).0 != name
                                    && crate::peer::explicit_bindings(&key).is_some_and(
                                        |bindings| !bindings.contains_key(name.as_str()),
                                    );
                            if !supplied_by_package {
                                continue;
                            }
                        }
                        let pin = v.as_str().ok_or_else(|| {
                            reconcile::missing(
                                &[&key, group, name],
                                "expected a resolved reference",
                            )
                        })?;
                        let target = if pin.starts_with("link:") {
                            pin.to_owned()
                        } else {
                            graph::snapshot_key(name, pin, graph::section(self.old, "snapshots"))
                        };
                        if !target.starts_with("link:") {
                            self.pending.insert(target.clone());
                        }
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
            } else {
                if !self.cache.releases.contains_key(base)
                    && RegistryPackage::from_key(base).is_none()
                    && let Some(locator) = manifest::selector(base).1
                {
                    let actual = self.cache.other(".", manifest::selector(base).0, locator)?;
                    if actual != base {
                        return Err(reconcile::missing(
                            &[base],
                            format!("source manifest identifies {actual}"),
                        ));
                    }
                }
                let release = Rc::clone(self.cache.release(base)?);
                let mut resolution = std::borrow::Cow::Borrowed(&release.resolution);
                if local.is_none()
                    && let Some(hash) = record["resolution"].get("integrity")
                    && release.resolution.get("integrity") != Some(hash)
                {
                    // Older locks can carry the same tarball's SHA-1 even when
                    // the registry now publishes a stronger default digest.
                    if !self
                        .cache
                        .published_sha1
                        .get(base)
                        .is_some_and(|sha| Some(sha.as_str()) == hash.as_str())
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
                let bundle = raw
                    .get("bundledDependencies")
                    .or_else(|| raw.get("bundleDependencies"));
                // pnpm 9.15.1 removed a package's own name from its peer requirements.
                let exclude_self = self
                    .config
                    .pnpm_version
                    .as_ref()
                    .is_none_or(|v| v >= &node_semver::Version::new(9, 15, 1));
                let own_name = raw["name"].as_str();
                for group in ["dependencies", "optionalDependencies", "peerDependencies"] {
                    let Some(entries) = raw.get(group) else {
                        continue;
                    };
                    for (name, value) in entries.as_object().ok_or_else(|| {
                        reconcile::missing(&[base, group], "expected a dependency mapping")
                    })? {
                        let value = value.as_str().ok_or_else(|| {
                            reconcile::missing(
                                &[base, group, name],
                                "expected a dependency selector",
                            )
                        })?;
                        let replacement = self.base.rules.select(raw, name, value)?;
                        let spec = match replacement {
                            Some(value) => self.base.rules.resolve(&dir, name, value)?,
                            None => Some(std::borrow::Cow::Borrowed(value)),
                        };
                        let Some(spec) = spec else {
                            continue;
                        };
                        if group == "peerDependencies"
                            && (replacement.is_none() || manifest::valid_peer_range(&spec))
                        {
                            if exclude_self && own_name == Some(name) {
                                continue;
                            }
                            record[group][name] = spec.as_ref().into();
                            if raw["peerDependenciesMeta"][name]["optional"] == true {
                                record["peerDependenciesMeta"][name] = json!({"optional":true});
                            }
                            continue;
                        }
                        if (group == "dependencies"
                            && raw["optionalDependencies"].get(name).is_some())
                            || (group == "optionalDependencies"
                                && self
                                    .config
                                    .resolution
                                    .ignored_optional_dependencies
                                    .as_ref()
                                    .and_then(Value::as_array)
                                    .is_some_and(|a| a.iter().any(|v| v == name)))
                            || bundle.is_some_and(|v| {
                                v == true
                                    || v.as_array().is_some_and(|a| a.iter().any(|v| v == name))
                            })
                        {
                            continue;
                        }
                        let group = if group == "peerDependencies" {
                            "dependencies"
                        } else {
                            group
                        };
                        let old =
                            graph::dependency(original, name).map(|(_, v)| json!({"version":v}));
                        let target = self.select(
                            Owner::Snapshot(key.clone()),
                            &dir,
                            raw,
                            name,
                            &spec,
                            old.as_ref(),
                        )?;
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
                // pnpm interprets optional metadata without a declared peer range
                // as "*", unless the package installs that name itself. Releases
                // that record fallback peers keep the name when it is an ordinary
                // dependency, and drop it only for an optional dependency.
                let fallback = super::peers::records_fallback_peers(self.config);
                for (name, meta) in graph::section(raw, "peerDependenciesMeta")
                    .into_iter()
                    .flatten()
                {
                    if meta["optional"] == true
                        && raw["peerDependencies"].get(name).is_none()
                        && !(exclude_self && own_name == Some(name))
                        && raw["optionalDependencies"].get(name).is_none()
                        && (fallback || raw["dependencies"].get(name).is_none())
                    {
                        record["peerDependencies"][name] = "*".into();
                        record["peerDependenciesMeta"][name] = json!({"optional":true});
                    }
                }
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
                historical_edges_match: Default::default(),
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
