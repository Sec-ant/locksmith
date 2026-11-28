//! Lockfile data model, workspace rules, schema helpers, and graph validation.

pub(crate) mod error;
pub(crate) mod registry;

pub(crate) mod catalog;
pub(crate) mod checksum;
pub(crate) mod patch;
pub(crate) mod pin;
pub(crate) mod schema;

pub(crate) mod config;
pub(crate) mod graph;
pub(crate) mod lockfile;
pub(crate) mod manifest;
pub(crate) mod workspace_links;
