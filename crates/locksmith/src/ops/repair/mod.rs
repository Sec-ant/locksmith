//! Repair an existing lock without preferring an earlier branch's legal pins.
mod evidence;
mod importers;
mod peers;

use evidence::Evidence;

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    DocumentKind, Lockfile, LockfileError, RegistryMetadata, Value, WorkspaceConfig,
    model::error::missing,
    model::graph,
    model::pin,
    resolve::{self, MergeContext},
};

/// Final workspace inputs and optional evidence for missing records.
/// Parents are (lock, effective configuration) pairs. Their order does not
/// authorize replacing a valid pin in the current lock.
#[derive(Debug, Clone, Copy)]
pub struct RepairContext<'a> {
    /// Final manifests keyed by importer path, including the root at `.`.
    pub manifests: &'a BTreeMap<String, Value>,
    /// Effective configuration for the lock being repaired.
    pub configuration: &'a WorkspaceConfig,
    /// Patch contents keyed by declared workspace-relative path.
    pub patches: &'a BTreeMap<String, String>,
    /// Additional locked graphs and the effective configuration of each graph.
    pub parents: &'a [(&'a Lockfile, &'a WorkspaceConfig)],
    /// Optional published metadata supplied by the caller for missing resolutions.
    pub registry: Option<&'a RegistryMetadata>,
}

/// A parsed repair source that can be reused when more evidence becomes available.
#[derive(Debug, Clone)]
pub struct RepairInput(Lockfile);

/// Repair the supplied YAML against the final workspace. A healthy input is
/// returned byte-for-byte. Missing or ambiguous evidence produces an error.
/// Records outside the workspace dependency closure are preserved. Use
/// [`Lockfile::validate`] to check references in the entire output.
///
/// # Examples
/// ```
/// use std::collections::BTreeMap;
/// use locksmith::{RepairContext, Value, WorkspaceConfig, repair};
///
/// let mut manifests = BTreeMap::new();
/// manifests.insert(".".into(), Value::Object(Default::default()));
/// let configuration = WorkspaceConfig::default();
/// let context = RepairContext {
///     manifests: &manifests,
///     configuration: &configuration,
///     patches: &BTreeMap::new(),
///     parents: &[],
///     registry: None,
/// };
/// let repaired = repair("lockfileVersion: '9.0'\nimporters:\n  .: {}\n", &context)?;
/// assert!(repaired.project().is_some());
/// # Ok::<(), locksmith::LockfileError>(())
/// ```
///
/// # Errors
/// Invalid YAML or an unsupported format fails during parsing. Incompatible settings,
/// missing resolutions or ambiguous repairs return [`LockfileError::ResolutionRequired`].
/// Graph and output errors are returned when the repair cannot be verified.
pub fn repair(text: &str, context: &RepairContext<'_>) -> Result<Lockfile, LockfileError> {
    RepairInput::parse(text)?.apply(context)
}

impl std::str::FromStr for RepairInput {
    type Err = LockfileError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl RepairInput {
    /// Read the pnpm version from the bootstrap document even if the following
    /// project document needs repair.
    #[must_use]
    pub fn recorded_pnpm_version(text: &str) -> Option<node_semver::Version> {
        crate::resolve::recovery::bootstrap_lockfile(text)?
            .0
            .recorded_pnpm_version()
    }

    /// Accept the same YAML and identical duplicate records as [`repair`].
    ///
    /// # Errors
    /// Malformed YAML, unsupported versions or conflicting duplicate records return an error.
    pub fn parse(text: &str) -> Result<Self, LockfileError> {
        Lockfile::parse_for_repair(text, None).map(Self)
    }

    /// Use a reconciled lock to select an existing copy of a conflicting
    /// project snapshot. Each selected record must already appear in the input.
    ///
    /// # Errors
    /// Returns an error when the input cannot be parsed or a duplicated snapshot has no unambiguous match.
    pub fn parse_with_snapshot_reference(
        text: &str,
        reference: &Lockfile,
    ) -> Result<Self, LockfileError> {
        Lockfile::parse_for_repair(text, reference.project()).map(Self)
    }

    /// Reconcile the source projects to select existing copies of conflicting snapshots.
    /// The repaired lock retains its own bootstrap document for validation by [`Self::apply`].
    ///
    /// # Errors
    /// Returns a reconciliation error or an error for invalid input or unmatched duplicate snapshots.
    pub fn parse_with_merge_witness(
        text: &str,
        locks: [&Lockfile; 3],
        context: &MergeContext<'_>,
    ) -> Result<Self, LockfileError> {
        let witness = resolve::resolve_project(locks, context)?;
        Self::parse_with_snapshot_reference(text, &witness)
    }

    /// Check the source's bootstrap against the current workspace.
    ///
    /// # Errors
    /// Returns an error if the root manifest is missing or the bootstrap is invalid.
    pub fn validate_bootstrap(&self, context: &RepairContext<'_>) -> Result<(), LockfileError> {
        let lock = &self.0;
        let root = context
            .manifests
            .get(".")
            .ok_or_else(|| missing(&["manifests", "."], "the root manifest is required"))?;
        if !context
            .configuration
            .matches_config_dependencies(lock.environment())
        {
            return Err(missing(
                &["importers", ".", "configDependencies"],
                "bootstrap graph does not match the configuration dependencies",
            ));
        }
        if let Some(env) = lock.environment() {
            let issues = graph::validate_with_config(
                env,
                DocumentKind::Environment,
                Some(context.configuration),
            );
            if !issues.is_empty() {
                return Err(LockfileError::InvalidGraph(issues));
            }
        }
        lock.check_package_manager(root, Some(context.configuration))
    }

    /// Repair the original source against this context.
    ///
    /// # Errors
    /// Returns the workspace, graph and resolution errors described by [`repair`].
    pub fn apply(&self, context: &RepairContext<'_>) -> Result<Lockfile, LockfileError> {
        let lock = &self.0;
        let doc = lock
            .project()
            .ok_or_else(|| missing(&[], "missing project document"))?;
        self.validate_bootstrap(context)?;
        let mut evidence = Evidence::new(lock, context);
        let current = (lock, context.configuration);
        let sources = [
            current,
            context.parents.first().copied().unwrap_or(current),
            context.parents.get(1).copied().unwrap_or(current),
        ];
        let docs = sources.map(|(lock, _)| {
            lock.project()
                .ok_or_else(|| missing(&["parents"], "expected a project lockfile"))
        });
        let [d, a, b] = docs;
        let metadata = resolve::metadata::resolve(
            [d?, a?, b?],
            [0, 1, 2],
            0,
            &evidence.merge_context(sources.map(|(_, cfg)| cfg)),
        )?;
        resolve::metadata::check_repair_configuration(doc, &metadata, context.configuration)?;
        let mut result = importers::repair(doc, &metadata, &mut evidence)?;
        let roots = context.manifests.keys().cloned().collect();
        loop {
            let closure = match graph::closure(&result, DocumentKind::Project, &roots) {
                Ok(closure) => closure,
                Err(_) => {
                    fill_missing(&mut result, &mut evidence)?;
                    graph::closure(&result, DocumentKind::Project, &roots)?
                }
            };
            let plan = crate::resolve::extensions::plan(
                &result,
                context.configuration,
                None,
                Some(&closure),
            )?;
            if !plan.is_empty() {
                plan.apply(result.to_mut());
                continue;
            }
            if matches!(result, Cow::Borrowed(_)) {
                break;
            }
            let edits = peers::affected(&result, &mut evidence)?;
            if edits.is_empty() {
                break;
            }
            for (id, group, name, pin) in edits {
                result.to_mut()["importers"][id][group][name]["version"] = Value::String(pin);
            }
        }
        drop(evidence);
        match result {
            Cow::Borrowed(_) => Ok(lock.clone()),
            Cow::Owned(doc) => lock.edit_project(&doc),
        }
    }
}

fn fill_missing(
    doc: &mut Cow<'_, Value>,
    evidence: &mut Evidence<'_>,
) -> Result<(), LockfileError> {
    let mut pending = BTreeSet::new();
    let mut seen = std::collections::HashSet::new();
    for (id, record) in graph::section(doc, "importers").into_iter().flatten() {
        enqueue(doc, "importers", id, record, &mut pending)?;
    }
    while let Some(key) = pending.pop_first() {
        if seen.contains(&key) {
            continue;
        }
        if doc["snapshots"].get(&key).is_none() {
            doc.to_mut()["snapshots"][&key] = evidence.record("snapshots", &key)?;
        }
        let pkg = pin::package_key(&key);
        if doc["packages"].get(pkg).is_none() {
            doc.to_mut()["packages"][pkg] = evidence.record("packages", pkg)?;
        }
        enqueue(
            doc,
            "snapshots",
            &key,
            &doc["snapshots"][&key],
            &mut pending,
        )?;
        seen.insert(key);
    }
    Ok(())
}

fn enqueue(
    doc: &Value,
    section: &str,
    id: &str,
    record: &Value,
    pending: &mut BTreeSet<String>,
) -> Result<(), LockfileError> {
    let mut issues = Vec::new();
    let snapshots = graph::section(doc, "snapshots");
    for (_, name, version) in graph::references(
        record,
        DocumentKind::Project,
        section,
        id,
        true,
        &mut issues,
    ) {
        if version.starts_with("link:") {
            continue;
        }
        let key = graph::snapshot_key(name, version, snapshots);
        let key = if !snapshots.is_some_and(|m| m.contains_key(&key))
            && crate::RegistryRequest::manifest(version).is_some()
        {
            version.to_owned()
        } else {
            key
        };
        pending.insert(key);
    }
    if !issues.is_empty() {
        return Err(LockfileError::InvalidGraph(issues));
    }
    Ok(())
}
