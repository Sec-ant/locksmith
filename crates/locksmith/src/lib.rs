//! Merge pnpm lockfiles, repair a committed graph or converge an installed candidate.
//!
//! [`merge`] accepts the ancestor and both branch locks. Pass `None` for a three-way
//! comparison or `Some(&context)` to select records using the final workspace settings
//! and manifests. [`converge`] retains changes required by manifest and workspace edits and
//! restores unrelated baseline pins. [`repair`] fixes broken records against final workspace inputs.
//! Call [`inspect_peers`] to apply peer policy.
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

mod assembly;
mod checksum;
mod config;
mod diagnostics;
mod error;
mod extensions;
mod graph;
mod lockfile;
mod manifest;
mod merge;
mod peer;
mod reconcile;
mod recovery;
mod registry;
mod repair;
mod scope;
mod source;
mod verify;

pub use config::{
    ExtensionChecksum, LinkedPeerEncoding, LockSettings, OptionalPeerScope, OverrideTieBreak,
    PatchEncoding, ResolutionSettings, WorkspaceConfig,
};
pub use diagnostics::{PeerIssue, PeerRules, PeerVersionMatching, inspect_peers};
pub use error::{Conflict, ConflictKind, LockfileError, ValidationIssue};
pub use graph::DependencyClosure;
pub use lockfile::{DocumentKind, Lockfile};
pub use merge::{MergeInput, merge};
pub use reconcile::{MergeContext, MergeTarget};
pub use registry::{RegistryMetadata, RegistryRequest};
pub use repair::{RepairContext, repair};
pub use scope::{ConvergeOptions, ManifestChange, converge};
pub use serde_json::Value;
