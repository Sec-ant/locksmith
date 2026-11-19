//! pnpm version conventions and peer acceptance used by filesystem adapters.
use std::error::Error;

use locksmith::{
    LinkedPeerEncoding, Lockfile, LockfileError, OptionalPeerScope, OverrideTieBreak,
    PatchEncoding, PeerRules, PeerVersionMatching, WorkspaceConfig,
};
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
        let mut config = WorkspaceConfig::from_pnpm(raw)?;
        if let Some(v) = &self.0 {
            config.extension_checksum = Some(if v.major < 10 {
                locksmith::ExtensionChecksum::Md5
            } else {
                locksmith::ExtensionChecksum::Sha256
            });
            if v.major >= 12 {
                config.override_tie_break = OverrideTieBreak::First;
            }
            if v.major < 10 {
                config.patch_encoding = PatchEncoding::Md5Base32WithPath;
            }
            if v.major >= 11 {
                config.linked_peer_encoding = LinkedPeerEncoding::Path;
                config.patch_encoding = PatchEncoding::Sha256;
            }
            if v.major < 11
                || (v.major == 11 && v < &node_semver::Version::new(11, 28, 12))
                || (v.major == 12 && v < &node_semver::Version::new(12, 19, 0))
            {
                config.resolution.optional_peer_scope = OptionalPeerScope::Workspace;
            }
        }
        Ok(config)
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
