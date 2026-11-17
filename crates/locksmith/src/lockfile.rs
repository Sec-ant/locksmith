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
}

impl Lockfile {
    /// Parse YAML, rejecting duplicate keys and unsupported format versions.
    ///
    /// Graph validation is separate: [`Self::validate`] checks all references.
    /// This lets a graft ignore unrelated, stale records in its donor.
    pub fn parse(text: &str) -> Result<Self, LockfileError> {
        let source = Arc::new(Source::parse(text)?);
        Ok(Self {
            documents: source.documents.clone(),
            sources: vec![source],
        })
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
        let mut issues = Vec::new();
        for (&kind, document) in &self.documents {
            issues.extend(graph::validate(document, kind));
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
        for source in &self.sources {
            if source.documents == self.documents {
                return Ok(source.text.to_string());
            }
        }
        let mut output = String::new();
        for (&kind, document) in &self.documents {
            if kind == DocumentKind::Environment || !output.is_empty() {
                output.push_str("---\n");
            }
            let start = output.len();
            crate::source::render(kind, document, &self.sources, &mut output)?;
            crate::verify::verify(&output[start..], document)?;
        }
        Ok(output)
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
        Self { documents, sources }
    }
}
