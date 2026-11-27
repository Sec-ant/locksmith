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

#![warn(missing_docs)]

// Layer 0: Foundation
#[cfg(feature = "bench-profile")]
#[doc(hidden)]
pub mod profile;

mod error;
mod registry;

// Layer 1: Schema & Checksums
mod catalog;
mod patch;

mod checksum;
mod schema;

mod pin;

// Layer 2: YAML & Source
mod source;
mod verify;

// Layer 3: Workspace Rules, Graph & Lockfile
mod config;
mod workspace_links;

mod manifest;

mod graph;
mod lockfile;

// Layer 4: Diagnostics & Inspection
mod diagnostics;
mod inspect;

// Layer 5: Peer & Resolution Engines
mod peer;
mod reconcile;
mod recovery;

// Layer 6: Public Workflows
mod converge;
mod repair;

mod merge;

pub use config::{
    ExtensionChecksum, LinkedPeerEncoding, LockSettings, OptionalPeerScope, OverrideTieBreak,
    PatchEncoding, ResolutionSettings, WorkspaceConfig,
};
pub use converge::{
    ConvergeOptions, ManifestChange, converge, converge_with_patches,
    converge_with_recorded_baselines,
};
pub use diagnostics::{
    Disposition, PeerIssue, PeerRules, PeerStatus, PeerVersionMatching, UnknownReason,
    inspect_peers,
};
pub use error::{Conflict, ConflictKind, LockfileError, ValidationIssue};
pub use graph::DependencyClosure;
pub use inspect::{
    ExternalLink, Inspection, InspectionSummary, LinkSource, LockedRelease, PackageInstances,
    PeerBinding, PeerGroup, PeerRequirement, inspect,
};
pub use lockfile::{DocumentKind, Lockfile};
pub use merge::{ContextualMerge, MergeInput, merge};
pub use reconcile::{MergeContext, MergeTarget};
pub use registry::{RegistryMetadata, RegistryRequest};
pub use repair::{RepairContext, RepairInput, repair};
pub use serde_json::Value;
