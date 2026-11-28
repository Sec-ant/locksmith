//! Lockfile YAML parsing, source-span editing, formatting, verification, and duplicate-key recovery.

pub(crate) mod edit;
pub(crate) mod format;
mod implicit;
pub(crate) mod recovery;
mod scalars;
pub(crate) mod source;
pub(crate) mod verify;
