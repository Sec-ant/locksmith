//! pnpm version conventions and peer acceptance used by filesystem adapters.
use std::error::Error;

use locksmith::{Lockfile, LockfileError, PeerRules, PeerVersionMatching, WorkspaceConfig};
use serde_json::Value;

pub struct Policy(Option<node_semver::Version>);

impl Policy {
    pub fn from_manifest(manifest: &Value) -> Self {
        Self(
            manifest["packageManager"]
                .as_str()
                .and_then(|name| name.strip_prefix("pnpm@"))
                .and_then(|version| version.parse().ok()),
        )
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
        let matching = if self.0.as_ref().is_some_and(|v| (9..12).contains(&v.major)) {
            PeerVersionMatching::ReleaseCompatible
        } else {
            PeerVersionMatching::Semver
        };
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
