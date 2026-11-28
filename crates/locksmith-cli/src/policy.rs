//! pnpm version conventions and peer acceptance used by filesystem adapters.
use std::error::Error;

use locksmith::{Lockfile, LockfileError, PeerRules, PeerVersionMatching, WorkspaceConfig};
use serde_json::Value;

use crate::ui::{Ui, plural};

pub struct Policy(Option<node_semver::Version>);

impl Policy {
    pub fn from_configuration(config: &WorkspaceConfig) -> Self {
        Self(config.pnpm_version.clone())
    }

    pub fn from_project(project: &crate::workspace::Project) -> Self {
        Self(project.pnpm_version.clone())
    }

    pub fn check_peers(
        &self,
        raw: &Value,
        result: &Lockfile,
        manifests: &std::collections::BTreeMap<String, Value>,
        ui: &Ui,
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
            ui.warning(format!(
                "{} {} allowed by the configured policy",
                issues.len(),
                plural(issues.len(), "peer issue", "peer issues")
            ));
        }
        Ok(())
    }
}
