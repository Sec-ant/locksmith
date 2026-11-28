use std::fmt;

use crate::{DocumentKind, Value};

/// A disagreement requiring the caller to select a coherent dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Conflict {
    /// Document in which the records disagree.
    pub document: DocumentKind,
    /// YAML mapping keys, kept as separate segments (package names contain `/`).
    pub path: Vec<String>,
    /// Why the available changes could not be combined.
    pub kind: ConflictKind,
    /// Common ancestor value, absent for a newly added key.
    pub base: Option<Value>,
    /// Value from the first branch, absent after a deletion.
    pub ours: Option<Value>,
    /// Value from the second branch, absent after a deletion.
    pub theirs: Option<Value>,
}

/// Reason a three-way graph merge requires a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConflictKind {
    /// Both branches supplied incompatible values.
    BothChanged,
    /// One branch deleted a key that the other modified.
    DeleteModify,
    /// A candidate record differs from a record that a retained dependency needs.
    SharedDependency,
}

/// An invalid reference or malformed lockfile field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ValidationIssue {
    /// Document containing the invalid record.
    pub document: DocumentKind,
    /// Mapping keys from the document root to the problem.
    pub path: Vec<String>,
    /// Description of the invalid field or reference.
    pub message: String,
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BothChanged => "incompatible changes",
            Self::DeleteModify => "deletion conflicts with a modification",
            Self::SharedDependency => "incompatible shared dependency",
        })
    }
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {}: {}",
            self.document,
            display_path(&self.path),
            self.kind
        )
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {}: {}",
            self.document,
            display_path(&self.path),
            self.message
        )
    }
}

fn display_path(path: &[String]) -> String {
    if path.is_empty() {
        "<root>".into()
    } else {
        path.join(" / ")
    }
}

fn first_issue<T: fmt::Display>(issues: &[T]) -> String {
    issues
        .first()
        .map_or_else(|| "none".into(), ToString::to_string)
}

pub(crate) fn issue(
    kind: DocumentKind,
    path: &[&str],
    message: impl Into<String>,
) -> ValidationIssue {
    ValidationIssue {
        document: kind,
        path: path.iter().map(|s| (*s).into()).collect(),
        message: message.into(),
    }
}

/// Input, resolution and output failures reported by lockfile operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockfileError {
    /// Optional acceptance-layer rejection. Graph operations do not emit this.
    #[error("peer policy rejects {} issues; first: {}", .0.len(), first_issue(.0))]
    UnmetPeers(Vec<PeerIssue>),
    /// No unique compatible resolution follows from the available evidence.
    #[error("resolution requires additional input at {}: {message}", display_path(.path))]
    ResolutionRequired {
        /// Mapping keys identifying the unresolved dependency or record.
        path: Vec<String>,
        /// Description of the missing evidence or mismatch.
        message: String,
        /// A package metadata lookup that can advance this resolution.
        request: Option<Box<crate::RegistryRequest>>,
    },
    /// The input could not be parsed as unambiguous YAML.
    #[error("invalid YAML: {0}")]
    Parse(#[source] Box<serde_saphyr::Error>),
    /// The document uses an unsupported lockfile format.
    #[error("unsupported lockfileVersion {version}; expected 9.0 (pnpm 9–12 default format)")]
    UnsupportedVersion {
        /// Value of the unsupported lockfileVersion field.
        version: Value,
    },
    /// A required lockfile structure is absent or malformed.
    #[error("invalid lockfile: {0}")]
    InvalidDocument(String),
    /// Recorded dependency edges or package metadata failed validation.
    #[error("invalid dependency graph ({} issues); first: {}", .0.len(), first_issue(.0))]
    InvalidGraph(Vec<ValidationIssue>),
    /// The branches have incompatible changes that affect the result.
    #[error("lockfile merge has {} conflicts; first: {}", .0.len(), first_issue(.0))]
    Conflicts(Vec<Conflict>),
    /// A supplied workspace manifest is absent where required or has an invalid shape.
    #[error("expected package.json objects before and/or after the change to {0:?}")]
    InvalidManifest(String),
    /// A locked importer disagrees with its corresponding manifest or configuration.
    #[error("{side} lockfile does not match manifest {importer:?}: {message}")]
    ManifestMismatch {
        /// Workspace-relative project path.
        importer: String,
        /// Input stage whose lock disagrees with the manifest.
        side: &'static str,
        /// Description of the missing evidence or mismatch.
        message: String,
    },
    /// A workspace-wide setting changed outside the supported convergence rules.
    #[error("global configuration {field:?} changed in {importer:?} during convergence")]
    GlobalConfigurationChanged {
        /// Importer containing the changed declaration.
        importer: String,
        /// Name of the changed configuration field.
        field: String,
    },
    /// The caller could not read a workspace input needed for validation.
    #[error("could not read workspace input: {0}")]
    WorkspaceInput(String),
    /// Rendered bytes did not reproduce the intended lockfile data.
    #[error("serialized lockfile does not reproduce its data")]
    OutputMismatch,
}

impl From<serde_saphyr::Error> for LockfileError {
    fn from(error: serde_saphyr::Error) -> Self {
        Self::Parse(Box::new(error))
    }
}

/// An unmet peer requirement. The caller decides whether this rejects a result.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct PeerIssue {
    /// Requester snapshot ID, including its peer suffix.
    pub snapshot: String,
    /// Dependency name from the requesting package’s peer declarations.
    pub peer: String,
    /// Declared peer range or specifier.
    pub wanted: String,
    /// Locked provider reference, or `None` when the peer has no binding.
    pub resolved: Option<String>,
}

impl fmt::Display for PeerIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} requires {}@{}",
            self.snapshot, self.peer, self.wanted
        )?;
        match &self.resolved {
            Some(reference) => write!(f, " (locked to {reference})"),
            None => f.write_str(" (no provider)"),
        }
    }
}

pub(crate) fn missing(path: &[&str], message: impl Into<String>) -> LockfileError {
    LockfileError::ResolutionRequired {
        request: None,
        path: path.iter().map(|s| (*s).into()).collect(),
        message: message.into(),
    }
}

pub(crate) fn mismatch(
    importer: &str,
    side: &'static str,
    message: impl Into<String>,
) -> LockfileError {
    LockfileError::ManifestMismatch {
        importer: importer.into(),
        side,
        message: message.into(),
    }
}
