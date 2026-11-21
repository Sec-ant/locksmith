use std::collections::BTreeMap;

use super::Result;
use locksmith::{
    Lockfile, LockfileError, PeerRules, PeerVersionMatching, RegistryMetadata, RegistryRequest,
    Value, WorkspaceConfig,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;

type Manifests = BTreeMap<String, Value>;
type Patches = BTreeMap<String, String>;

pub(super) fn decode<T: DeserializeOwned>(raw: Value) -> Result<T> {
    serde_json::from_value(raw).map_err(|error| LockfileError::InvalidDocument(error.to_string()))
}

fn configuration(raw: &Value, manifest: Option<&Value>) -> Result<WorkspaceConfig> {
    let manager = raw
        .get("packageManager")
        .or_else(|| manifest?.get("packageManager"));
    match manager {
        None => WorkspaceConfig::from_pnpm(raw),
        Some(manager) => {
            let version = manager
                .as_str()
                .and_then(|s| s.strip_prefix("pnpm@"))
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| {
                    LockfileError::InvalidDocument(
                        "packageManager must be an exact pnpm@version".into(),
                    )
                })?;
            WorkspaceConfig::from_pnpm_version(raw, &version)
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Request {
    Version { name: String, selector: String },
    Manifest { name: String, version: String },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Evidence {
    Published {
        request: Request,
        manifest: Value,
        registry: String,
        #[serde(default, rename = "includeTarball")]
        include_tarball: bool,
    },
    Unavailable {
        #[serde(rename = "unavailableManifest")]
        unavailable: Unavailable,
    },
}

#[derive(Deserialize)]
struct Unavailable {
    name: String,
    version: String,
}

fn registry(entries: Vec<Evidence>) -> Result<Option<RegistryMetadata>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut metadata = RegistryMetadata::default();
    for entry in entries {
        match entry {
            Evidence::Published {
                request,
                manifest,
                registry,
                include_tarball,
            } => {
                let request = match request {
                    Request::Version { name, selector } => {
                        RegistryRequest::Version { name, selector }
                    }
                    Request::Manifest { name, version } => {
                        RegistryRequest::Manifest { name, version }
                    }
                };
                metadata.insert(&request, manifest, &registry, include_tarball)?;
            }
            Evidence::Unavailable { unavailable } => {
                metadata.skip_manifest(&unavailable.name, &unavailable.version)
            }
        }
    }
    Ok(Some(metadata))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Target {
    Ours,
    Theirs,
}

#[derive(Deserialize)]
pub(super) struct Merge {
    target: Target,
    manifests: Manifests,
    configurations: [Value; 3],
    configuration: Value,
    #[serde(default)]
    patches: Patches,
    #[serde(default)]
    registry: Vec<Evidence>,
}

impl Merge {
    pub(super) fn apply(
        self,
        base: locksmith::MergeInput<'_>,
        ours: locksmith::MergeInput<'_>,
        theirs: locksmith::MergeInput<'_>,
    ) -> Result<Lockfile> {
        let root = self.manifests.get(".");
        let [b, o, t] = self
            .configurations
            .each_ref()
            .map(|raw| configuration(raw, root));
        let registry = registry(self.registry)?;
        locksmith::merge(
            base,
            ours,
            theirs,
            Some(&locksmith::MergeContext {
                target: match self.target {
                    Target::Ours => locksmith::MergeTarget::Ours,
                    Target::Theirs => locksmith::MergeTarget::Theirs,
                },
                manifests: &self.manifests,
                configurations: [&b?, &o?, &t?],
                configuration: &configuration(&self.configuration, root)?,
                patches: &self.patches,
                registry: registry.as_ref(),
            }),
        )
    }
}

#[derive(Deserialize)]
pub(super) struct Change {
    importer: String,
    before: Option<Value>,
    after: Option<Value>,
}
pub(super) type Changes = Vec<Change>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Converge {
    baseline_configuration: Option<Value>,
    configuration: Option<Value>,
    #[serde(default)]
    patches: Patches,
    #[serde(default)]
    registry: Vec<Evidence>,
}

impl Converge {
    pub(super) fn apply(
        self,
        baseline: &Lockfile,
        candidate: &Lockfile,
        changes: &[Change],
    ) -> Result<Lockfile> {
        let root = changes.iter().find(|c| c.importer == ".");
        let before = self
            .baseline_configuration
            .as_ref()
            .map(|raw| configuration(raw, root.and_then(|c| c.before.as_ref())))
            .transpose()?;
        let after = self
            .configuration
            .as_ref()
            .map(|raw| configuration(raw, root.and_then(|c| c.after.as_ref())))
            .transpose()?;
        let registry = registry(self.registry)?;
        let changes: Vec<_> = changes
            .iter()
            .map(|c| locksmith::ManifestChange {
                importer: &c.importer,
                before: c.before.as_ref(),
                after: c.after.as_ref(),
            })
            .collect();
        locksmith::converge_with_patches(
            baseline,
            candidate,
            &changes,
            locksmith::ConvergeOptions {
                baseline_configuration: before.as_ref(),
                configuration: after.as_ref(),
                registry: registry.as_ref(),
            },
            &self.patches,
        )
    }
}

#[derive(Deserialize)]
struct Parent {
    yaml: String,
    configuration: Value,
}

#[derive(Deserialize)]
pub(super) struct Repair {
    manifests: Manifests,
    configuration: Value,
    #[serde(default)]
    parents: Vec<Parent>,
    #[serde(default)]
    registry: Vec<Evidence>,
    #[serde(default)]
    patches: Patches,
}

impl Repair {
    pub(super) fn apply(self, yaml: &str) -> Result<Lockfile> {
        let root = self.manifests.get(".");
        let parents = self
            .parents
            .into_iter()
            .map(|p| {
                Ok((
                    Lockfile::try_from(p.yaml)?,
                    configuration(&p.configuration, root)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let parents: Vec<_> = parents.iter().map(|(lock, cfg)| (lock, cfg)).collect();
        let registry = registry(self.registry)?;
        locksmith::repair(
            yaml,
            &locksmith::RepairContext {
                manifests: &self.manifests,
                configuration: &configuration(&self.configuration, root)?,
                patches: &self.patches,
                parents: &parents,
                registry: registry.as_ref(),
            },
        )
    }
}

#[derive(Deserialize)]
pub(super) struct PeerOptions {
    configuration: Option<Value>,
    manifests: Option<Manifests>,
}

impl PeerOptions {
    fn rules(&self, lock: &Lockfile, fallback: PeerVersionMatching) -> Result<PeerRules> {
        let empty = json!({});
        let raw = self.configuration.as_ref().unwrap_or(&empty);
        let config = configuration(raw, self.manifests.as_ref().and_then(|ms| ms.get(".")))?;
        let version = config.pnpm_version.or_else(|| lock.recorded_pnpm_version());
        let matching = version
            .as_ref()
            .map_or(fallback, |v| PeerVersionMatching::for_pnpm(Some(v)));
        PeerRules::from_pnpm(raw, matching)
    }

    pub(super) fn peers(self, lock: &Lockfile) -> Result<Vec<Value>> {
        lock.validate()?;
        let rules = self.rules(lock, PeerVersionMatching::Semver)?;
        Ok(super::diagnostic::peers(locksmith::inspect_peers(
            lock,
            &rules,
            self.manifests.as_ref(),
        )))
    }

    pub(super) fn report(self, lock: &Lockfile) -> Result<BTreeMap<String, Value>> {
        let rules = self.rules(lock, PeerVersionMatching::Unspecified)?;
        let report = locksmith::inspect(lock, &rules, self.manifests.as_ref())?;
        let Value::Object(report) = json!(report) else {
            unreachable!("inspection serializes as an object")
        };
        Ok(report.into_iter().collect())
    }
}
