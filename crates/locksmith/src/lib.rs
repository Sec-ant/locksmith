//! Merge pnpm lockfiles, repair a committed graph or converge an installed candidate.
//!
//! [`merge`] accepts the ancestor and both branch locks. Pass `None` for a three-way
//! comparison or `Some(&context)` to select records using the final workspace settings
//! and manifests. [`converge`] retains changes required by manifest and workspace edits and
//! restores unrelated baseline pins. [`repair`] fixes broken records against final workspace inputs.
//! Call [`inspect`] for peer bindings, workspace reachability and package instances.
//!
//! Supports the default `lockfileVersion: '9.0'` format written by pnpm 9–12,
//! including pnpm 11/12's separate environment document. Experimental lockfile
//! formats are rejected by their format version, independently of pnpm's version.
//! The library performs no filesystem, network, Git, or package-manager operations.
//!
//! ```
//! use locksmith::{Lockfile, merge};
//!
//! let base = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}}\n")?;
//! let ours = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}, lib: {}}\n")?;
//! let merged = merge(&base, &ours, &base, None)?;
//! assert!(merged.project().unwrap()["importers"].get("lib").is_some());
//! # Ok::<(), locksmith::LockfileError>(())
//! ```

#![warn(missing_docs, missing_debug_implementations)]

#[cfg(feature = "bench-profile")]
#[doc(hidden)]
pub mod profile;

// Five-layer architecture (strict DAG: model -> yaml -> peer -> resolve -> ops)
mod model;
mod ops;
mod peer;
mod resolve;
mod yaml;

pub use model::config::{
    ExtensionChecksum, LinkedPeerEncoding, LockSettings, OptionalPeerScope, OverrideTieBreak,
    PatchEncoding, ResolutionSettings, WorkspaceConfig,
};
pub use model::error::{Conflict, ConflictKind, LockfileError, ValidationIssue};
pub use model::graph::DependencyClosure;
pub use model::lockfile::{DocumentKind, Lockfile};
pub use model::registry::{RegistryMetadata, RegistryRequest};
pub use node_semver::Version;
pub use ops::converge::{ConvergeOptions, ManifestChange, converge};
pub use ops::inspect::peers::{
    Disposition, PeerIssue, PeerRules, PeerStatus, PeerVersionMatching, UnknownReason,
    inspect_peers,
};
pub use ops::inspect::{
    ExternalLink, Inspection, InspectionSummary, LinkSource, LockedRelease, PackageInstances,
    PeerBinding, PeerGroup, PeerRequirement, inspect,
};
pub use ops::merge::{ContextualMerge, MergeInput, merge};
pub use ops::repair::{RepairContext, RepairInput, repair};
pub use resolve::{MergeContext, MergeTarget};
pub use serde_json::{Map, Value};
