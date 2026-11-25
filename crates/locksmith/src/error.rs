use crate::{DocumentKind, Value};

/// A disagreement requiring the caller to select a coherent dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub document: DocumentKind,
    /// YAML mapping keys, kept as separate segments (package names contain `/`).
    pub path: Vec<String>,
    pub kind: ConflictKind,
    pub base: Option<Value>,
    pub ours: Option<Value>,
    pub theirs: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    BothChanged,
    DeleteModify,
    /// A candidate record differs from a record that a retained dependency needs.
    SharedDependency,
}

/// An invalid reference or malformed lockfile field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub document: DocumentKind,
    pub path: Vec<String>,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LockfileError {
    /// Optional acceptance-layer rejection. Graph operations do not emit this.
    #[error("peer policy rejects {} issues; first: {:?}", .0.len(), .0.first())]
    UnmetPeers(Vec<crate::PeerIssue>),
    #[error("resolution requires additional input at {path:?}: {message}")]
    ResolutionRequired {
        path: Vec<String>,
        message: String,
        /// A package metadata lookup that can advance this resolution.
        request: Option<Box<crate::RegistryRequest>>,
    },
    #[error("invalid YAML: {0}")]
    Parse(#[source] Box<serde_saphyr::Error>),
    #[error("unsupported lockfileVersion {version:?}; expected 9.0 (pnpm 9–12 default format)")]
    UnsupportedVersion { version: Value },
    #[error("invalid lockfile: {0}")]
    InvalidDocument(String),
    #[error("invalid dependency graph ({} issues); first: {:?}", .0.len(), .0.first())]
    InvalidGraph(Vec<ValidationIssue>),
    #[error("lockfile merge has {} conflicts; first path: {:?}", .0.len(), .0.first().map(|conflict| &conflict.path))]
    Conflicts(Vec<Conflict>),
    #[error("expected package.json objects before and/or after the change to {0:?}")]
    InvalidManifest(String),
    #[error("{side} lockfile does not match manifest {importer:?}: {message}")]
    ManifestMismatch {
        importer: String,
        side: &'static str,
        message: String,
    },
    #[error("global configuration {field:?} changed in {importer:?} during convergence")]
    GlobalConfigurationChanged { importer: String, field: String },
    #[error("could not read workspace input: {0}")]
    WorkspaceInput(String),
    #[error("could not serialize lockfile: {0}")]
    Serialize(#[source] serde_saphyr::SerializeError),
    #[error("serialized lockfile does not reproduce its data")]
    OutputMismatch,
}

impl From<serde_saphyr::Error> for LockfileError {
    fn from(error: serde_saphyr::Error) -> Self {
        Self::Parse(Box::new(error))
    }
}
