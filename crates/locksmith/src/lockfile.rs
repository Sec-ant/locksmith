use std::{collections::BTreeMap, sync::Arc};

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
                    crate::manifest::same_overrides(document, &source.documents[kind])
                        && (Arc::ptr_eq(document, &source.documents[kind])
                            || crate::source::SECTIONS.into_iter().chain(["catalogs"]).all(
                                |section| {
                                    source.reusable_field(
                                        *kind,
                                        section,
                                        self.preserve_baseline && i == 0,
                                    )
                                },
                            ))
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
