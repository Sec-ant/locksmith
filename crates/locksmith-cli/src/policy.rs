//! pnpm version conventions and peer acceptance used by filesystem adapters.
use std::error::Error;

use locksmith::{Lockfile, LockfileError, PeerRules, PeerVersionMatching, WorkspaceConfig};
use serde_json::Value;

pub struct Policy(Option<node_semver::Version>);

impl Policy {
    pub fn from_manifest(manifest: &Value) -> Self {
        Self(crate::workspace::pnpm_version(manifest))
    }

    pub fn from_project(project: &crate::workspace::Project) -> Self {
        Self(project.pnpm_version.clone())
    }

    pub fn configuration(&self, raw: &Value) -> Result<WorkspaceConfig, LockfileError> {
        match &self.0 {
            Some(v) => WorkspaceConfig::from_pnpm_version(raw, v),
            None => WorkspaceConfig::from_pnpm(raw),
        }
    }

    pub fn check_peers(
        &self,
        raw: &Value,
        result: &Lockfile,
        manifests: &std::collections::BTreeMap<String, Value>,
    ) -> Result<(), Box<dyn Error>> {
        let strict = match raw.get("strictPeerDependencies") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or("strictPeerDependencies must be a boolean")?,
        };
        let version = self.0.clone().or_else(|| result.recorded_pnpm_version());
        let matching = version.as_ref().map_or(PeerVersionMatching::Semver, |v| {
            PeerVersionMatching::for_pnpm(Some(v))
        });
        let rules = PeerRules::from_pnpm(raw, matching)?;
        let issues = locksmith::inspect_peers(result, &rules, Some(manifests));
        if strict && !issues.is_empty() {
            return Err(LockfileError::UnmetPeers(issues).into());
        }
        if !issues.is_empty() {
            eprintln!(
                "{} peer issues allowed by the configured policy",
                issues.len()
            );
        }
        Ok(())
    }
}
