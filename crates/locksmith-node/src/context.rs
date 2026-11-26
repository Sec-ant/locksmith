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
        let target = match self.target {
            Target::Ours => locksmith::MergeTarget::Ours,
            Target::Theirs => locksmith::MergeTarget::Theirs,
        };
        let (branches, config, merge) = target.workspace_configurations(
            root,
            [base, ours, theirs],
            self.configurations.each_ref(),
            &self.configuration,
        )?;
        let registry = registry(self.registry)?;
        merge.merge(&locksmith::MergeContext {
            target,
            manifests: &self.manifests,
            configurations: branches.each_ref(),
            configuration: &config,
            patches: &self.patches,
            registry: registry.as_ref(),
        })
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
            .map(|raw| {
                WorkspaceConfig::from_pnpm_context(
                    raw,
                    root.and_then(|c| c.before.as_ref()),
                    baseline.recorded_pnpm_version(),
                )
            })
            .transpose()?;
        let after = self
            .configuration
            .as_ref()
            .map(|raw| {
                WorkspaceConfig::from_pnpm_context(
                    raw,
                    root.and_then(|c| c.after.as_ref()),
                    candidate.recorded_pnpm_version(),
                )
            })
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
                let lock = Lockfile::try_from(p.yaml)?;
                let config = WorkspaceConfig::from_pnpm_source(
                    &p.configuration,
                    root,
                    lock.recorded_pnpm_version(),
                )?;
                Ok((lock, config))
            })
            .collect::<Result<Vec<_>>>()?;
        let parents: Vec<_> = parents.iter().map(|(lock, cfg)| (lock, cfg)).collect();
        let registry = registry(self.registry)?;
        locksmith::repair(
            yaml,
            &locksmith::RepairContext {
                manifests: &self.manifests,
                configuration: &WorkspaceConfig::from_pnpm_context(
                    &self.configuration,
                    root,
                    locksmith::RepairInput::recorded_pnpm_version(yaml),
                )?,
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
    fn rules(&self, lock: &Lockfile) -> Result<PeerRules> {
        let empty = json!({});
        let raw = self.configuration.as_ref().unwrap_or(&empty);
        let config = WorkspaceConfig::from_pnpm_context(
            raw,
            self.manifests.as_ref().and_then(|ms| ms.get(".")),
            lock.recorded_pnpm_version(),
        )?;
        let matching = PeerVersionMatching::for_pnpm(config.pnpm_version.as_ref());
        PeerRules::from_pnpm(raw, matching)
    }

    pub(super) fn peers(self, lock: &Lockfile) -> Result<Vec<Value>> {
        lock.validate()?;
        let rules = self.rules(lock)?;
        Ok(super::diagnostic::peers(locksmith::inspect_peers(
            lock,
            &rules,
            self.manifests.as_ref(),
        )))
    }

    pub(super) fn report(self, lock: &Lockfile) -> Result<BTreeMap<String, Value>> {
        let rules = self.rules(lock)?;
        let report = locksmith::inspect(lock, &rules, self.manifests.as_ref())?;
        let Value::Object(report) = json!(report) else {
            unreachable!("inspection serializes as an object")
        };
        Ok(report.into_iter().collect())
    }
}
