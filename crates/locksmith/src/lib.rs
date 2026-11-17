//! Converge and merge pnpm lockfiles without resolving new package versions.
//!
//! [`converge`] retains dependency changes and their required workspace/peer
//! context, checking manifest consistency and preserving unrelated resolutions.
//! [`merge`] combines three revisions using a common ancestor. Both use the graft
//! core to assemble and validate a coherent dependency graph. [`graft`] exposes
//! explicit whole-project transplantation for callers with their own scope policy.
//! Scoped operations remove newly unreachable packages and preserve old orphans.
//!
//! Supports the default `lockfileVersion: '9.0'` format written by pnpm 9–12,
//! including pnpm 11/12's separate environment document. Experimental lockfile
//! formats are rejected by their format version, independently of pnpm's version.
//! The library performs no filesystem, network, Git, or package-manager operations.
//!
//! ```
//! use locksmith::{GraftOptions, Lockfile, graft};
//!
//! let main = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}}\n")?;
//! let branch = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}, lib: {}}\n")?;
//! let merged = graft(&main, &branch, &GraftOptions::new(["lib"]))?;
//! assert!(merged.project().unwrap()["importers"].get("lib").is_some());
//! # Ok::<(), locksmith::LockfileError>(())
//! ```

mod error;
mod graft;
mod graph;
mod lockfile;
mod manifest;
mod merge;
mod scope;
mod source;
mod verify;

pub use error::{Conflict, ConflictKind, LockfileError, ValidationIssue};
pub use graft::{GraftOptions, graft};
pub use graph::DependencyClosure;
pub use lockfile::{DocumentKind, Lockfile};
pub use merge::merge;
pub use scope::{ManifestChange, converge};
pub use serde_json::Value;
