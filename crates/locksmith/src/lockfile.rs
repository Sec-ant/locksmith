use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::{LockfileError, Value, graph, source::Source};

/// The project dependency graph or pnpm's package-manager bootstrap graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DocumentKind {
    Environment,
    Project,
}

/// An immutable, parsed lockfile. Unknown metadata fields are retained.
#[derive(Debug, Clone)]
pub struct Lockfile {
    pub(crate) documents: BTreeMap<DocumentKind, Arc<Value>>,
    pub(crate) sources: Vec<Arc<Source>>,
    pub(crate) preserve_baseline: bool,
}

/// Parse an owned YAML document, reusing its text buffer for source blocks.
impl TryFrom<String> for Lockfile {
    type Error = LockfileError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let source = Arc::new(Source::parse(text)?);
        Ok(Self {
            documents: source.documents.clone(),
            sources: vec![source],
            preserve_baseline: false,
        })
    }
}

impl Lockfile {
    /// Parse YAML, rejecting duplicate keys and unsupported format versions.
    ///
    /// Graph validation is separate: [`Self::validate`] checks all references.
    /// Scoped operations can ignore unrelated, stale records in a candidate.
    pub fn parse(text: &str) -> Result<Self, LockfileError> {
        Self::try_from(text.to_owned())
    }

    pub fn project(&self) -> Option<&Value> {
        self.document(DocumentKind::Project)
    }

    pub fn environment(&self) -> Option<&Value> {
        self.document(DocumentKind::Environment)
    }

    pub fn document(&self, kind: DocumentKind) -> Option<&Value> {
        self.documents.get(&kind).map(AsRef::as_ref)
    }

    /// Package-manager version recorded in pnpm's bootstrap document, when present.
    pub fn recorded_pnpm_version(&self) -> Option<node_semver::Version> {
        self.environment()?["importers"]["."]["packageManagerDependencies"]["pnpm"]["version"]
            .as_str()?
            .parse()
            .ok()
    }

    /// Workspace manifests needed to determine the versions of linked peer providers.
    pub fn linked_peer_projects(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        let Some(doc) = self.project() else {
            return ids;
        };
        for (key, snapshot) in graph::section(doc, "snapshots").into_iter().flatten() {
            let package = &doc["packages"][graph::package_key(key)];
            for (name, _) in graph::section(package, "peerDependencies")
                .into_iter()
                .flatten()
            {
                if let Some(path) =
                    graph::dependency(snapshot, name).and_then(|(_, pin)| pin.strip_prefix("link:"))
                {
                    let id = graph::linked_project(doc.get("importers"), ".", path);
                    if doc["importers"].get(&id).is_some() {
                        ids.insert(id);
                    }
                }
            }
        }
        ids
    }

    /// Inspect a project's full dependency closure, including distinct peer
    /// instances. Catalog policy is checked separately by [`Self::validate`].
    pub fn dependency_closure(
        &self,
        importer: &str,
    ) -> Result<crate::DependencyClosure, LockfileError> {
        let document = self
            .project()
            .ok_or_else(|| LockfileError::InvalidDocument("missing project document".into()))?;
        graph::closure(
            document,
            DocumentKind::Project,
            &std::collections::BTreeSet::from([importer.into()]),
        )
    }

    /// Check all importer and snapshot edges, package records and catalog pins.
    /// Local `link:` references may refer to directories outside the workspace.
    pub fn validate(&self) -> Result<(), LockfileError> {
        self.validate_with_config(None)
    }

    /// Check the graph and compare its importers against the complete set of
    /// workspace manifests and configured patch files, keyed by declared path.
    /// External members are paths inside the workspace whose symlinks lead outside the root.
    /// Callers supply effective manifests if hooks apply. For a range pinned to a
    /// directory outside the member set, `read_linked` supplies its manifest,
    /// `None` for an absent directory, or an error for unreadable inputs.
    pub fn validate_workspace(
        &self,
        manifests: &BTreeMap<String, Value>,
        external_members: &BTreeSet<String>,
        config: &crate::WorkspaceConfig,
        patches: &BTreeMap<String, String>,
        read_linked: impl Fn(&str) -> Result<Option<Value>, LockfileError>,
    ) -> Result<(), LockfileError> {
        self.validate_with_config(Some(config))?;
        let document = self
            .project()
            .ok_or_else(|| LockfileError::InvalidDocument("missing project document".into()))?;
        let recorded = crate::LockSettings::from_document(document);
        let configured = &config.resolution.lock;
        if recorded.auto_install_peers != configured.auto_install_peers
            || recorded.dedupe_peers != configured.dedupe_peers
            || recorded.exclude_links_from_lockfile != configured.exclude_links_from_lockfile
        {
            return Err(crate::manifest::mismatch(
                ".",
                "workspace",
                "dependency settings differ",
            ));
        }
        crate::repair::check_recorded_settings(document, config)?;
        if let Some(catalogs) = document.get("catalogs") {
            for (catalog, entries) in catalogs.as_object().ok_or_else(|| {
                crate::manifest::mismatch(".", "workspace", "catalogs must be a mapping")
            })? {
                for (name, record) in entries.as_object().ok_or_else(|| {
                    crate::manifest::mismatch(
                        ".",
                        "workspace",
                        format!("catalogs/{catalog} must be a mapping"),
                    )
                })? {
                    let configured = config.catalogs.get(catalog).and_then(|c| c.get(name));
                    if !configured.is_some_and(|spec| record["specifier"] == spec.as_str()) {
                        return Err(crate::manifest::mismatch(
                            ".",
                            "workspace",
                            format!("catalogs/{catalog}/{name} specifier differs"),
                        ));
                    }
                }
            }
        }
        let patches_match = |encoding| {
            let expected = crate::reconcile::metadata::patch_records(config, patches, encoding)?;
            Ok::<_, LockfileError>(crate::reconcile::metadata::same_patch_records(
                document,
                expected.as_ref(),
            ))
        };
        if !patches_match(config.patch_encoding)?
            && !(config.pnpm_version.is_none()
                && (patches_match(crate::PatchEncoding::Md5Base32WithPath)?
                    || patches_match(crate::PatchEncoding::Sha256)?))
        {
            return Err(crate::manifest::mismatch(
                ".",
                "workspace",
                "patchedDependencies differs",
            ));
        }
        if !crate::checksum::extensions_match(config, document.get("packageExtensionsChecksum")) {
            return Err(crate::manifest::mismatch(
                ".",
                "workspace",
                "packageExtensionsChecksum differs",
            ));
        }
        if !config
            .resolution
            .ignored_optional_dependencies_match(document, false)?
        {
            return Err(crate::manifest::mismatch(
                ".",
                "workspace",
                "ignoredOptionalDependencies differs",
            ));
        }
        let effective =
            crate::reconcile::metadata::effective_overrides(config, manifests.get("."))?;
        let recorded_overrides = crate::manifest::rule_map(document, "overrides");
        if effective.as_ref() != recorded_overrides
            || effective
                .as_ref()
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .ne(recorded_overrides
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten())
        {
            return Err(crate::manifest::mismatch(
                ".",
                "workspace",
                "overrides differ",
            ));
        }
        let overrides = crate::manifest::Overrides::new(document, config);
        for id in graph::section(document, "importers")
            .into_iter()
            .flat_map(|m| m.keys())
        {
            if !manifests.contains_key(id) {
                crate::manifest::check(&overrides, id, None, "workspace")?;
            }
        }
        let workspace = crate::workspace_links::FrozenWorkspace::new(
            manifests,
            config,
            external_members,
            &read_linked,
        )?;
        for (id, manifest) in manifests {
            crate::manifest::check_with_links(
                &overrides,
                id,
                Some(manifest),
                "workspace",
                Some(&workspace),
            )?;
        }
        Ok(())
    }

    pub(crate) fn validate_with_config(
        &self,
        config: Option<&crate::WorkspaceConfig>,
    ) -> Result<(), LockfileError> {
        let mut issues = Vec::new();
        for (&kind, document) in &self.documents {
            issues.extend(graph::validate_with_config(document, kind, config));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(LockfileError::InvalidGraph(issues))
        }
    }

    /// Render YAML, reusing unchanged pnpm sections and individual entry blocks.
    ///
    /// An unchanged input is returned byte for byte, including CRLF and comments.
    /// Rewritten sections use pnpm's sorted entry order. Non-block YAML is emitted
    /// with the YAML serializer. Output is verified with a streaming comparison
    /// against the existing data, without building another document tree.
    pub fn to_yaml(&self) -> Result<String, LockfileError> {
        for (i, source) in self.sources.iter().enumerate() {
            if source.documents == self.documents
                && self.documents.iter().all(|(kind, document)| {
                    crate::manifest::same_override_order(document, &source.documents[kind])
                        && (Arc::ptr_eq(document, &source.documents[kind]) || {
                            let keep_importer_order = self.preserve_baseline
                                && crate::source::importer_order(&self.sources, *kind, document)
                                    .is_some();
                            crate::source::SECTIONS
                                .into_iter()
                                .chain(["catalogs"])
                                .all(|section| {
                                    source.reusable_field(
                                        *kind,
                                        section,
                                        self.preserve_baseline && i == 0,
                                        keep_importer_order,
                                    )
                                })
                        })
                })
            {
                return Ok(source.text.to_string());
            }
        }
        let mut output = String::new();
        for (&kind, document) in &self.documents {
            if kind == DocumentKind::Environment || !output.is_empty() {
                output.push_str("---\n");
            }
            let start = output.len();
            crate::source::render(
                kind,
                document,
                &self.sources,
                self.preserve_baseline,
                &mut output,
            );
            crate::verify::verify(&output[start..], document)?;
        }
        Ok(output)
    }

    pub(crate) fn converged_project(&self, project: Value, candidate: &Self) -> Self {
        let mut documents = self.documents.clone();
        documents.insert(DocumentKind::Project, Arc::new(project));
        let mut result = Self::with_documents(documents, &[self, candidate]);
        result.preserve_baseline = true;
        result
    }

    pub(crate) fn with_documents(
        documents: BTreeMap<DocumentKind, Arc<Value>>,
        inputs: &[&Self],
    ) -> Self {
        let mut sources = Vec::new();
        for input in inputs {
            for source in &input.sources {
                if !sources.iter().any(|existing| Arc::ptr_eq(existing, source)) {
                    sources.push(Arc::clone(source));
                }
            }
        }
        Self {
            documents,
            sources,
            preserve_baseline: false,
        }
    }
}
