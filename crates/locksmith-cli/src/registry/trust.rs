//! no-downgrade compares publication history in date order. Stable releases
//! are compared to earlier stable releases; prereleases see the whole history.
use super::{Packument, Registry, selection::excluded};
use crate::Result;
use jiff::Timestamp;
use node_semver::Version;
use serde_json::Value;
use std::collections::BTreeSet;

pub(super) trait PublicationTrust {
    fn rank(&self) -> Result<u8>;
}

impl PublicationTrust for Box<serde_json::value::RawValue> {
    fn rank(&self) -> Result<u8> {
        #[derive(serde::Deserialize)]
        struct Signature {
            #[serde(default, rename = "_npmUser")]
            user: Value,
            #[serde(default)]
            dist: Value,
        }
        let info: Signature = serde_json::from_str(self.get())?;
        Ok(if !info.user["approver"].is_null() {
            3
        } else if info.dist["attestations"]["provenance"].is_null() {
            0
        } else if !info.user["trustedPublisher"].is_null() {
            2
        } else {
            1
        })
    }
}

impl PublicationTrust for u8 {
    fn rank(&self) -> Result<u8> {
        Ok(*self)
    }
}

impl Registry {
    pub(super) fn accepts_locked<R: PublicationTrust>(
        &self,
        doc: &Packument<R>,
        v: &str,
        direct: bool,
    ) -> Result<bool> {
        Ok(doc.versions.contains_key(v)
            && self.trusted(doc, v)?
            && self.old_enough(doc, v)?
            && self.before_cutoff(doc, v, direct)?)
    }

    pub(super) fn trusted<R: PublicationTrust>(&self, doc: &Packument<R>, v: &str) -> Result<bool> {
        match self.config["trustPolicy"].as_str() {
            None | Some("off") => return Ok(true),
            Some("no-downgrade") => (),
            Some(other) => return Err(format!("unsupported trustPolicy {other}").into()),
        }
        if excluded(&self.config["trustPolicyExclude"], &doc.name, v)? {
            return Ok(true);
        }
        if doc.time.is_empty() && self.config["minimumReleaseAgeIgnoreMissingTime"] == true {
            return Ok(true);
        }
        let date = doc
            .time
            .get(v)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing trust history for {}@{v}", doc.name))?
            .parse::<Timestamp>()?;
        if let Some(minutes) = self.config["trustPolicyIgnoreAfter"].as_u64()
            && self.now.saturating_sub(date.as_second()).max(0) as u64 / 60 > minutes
        {
            return Ok(true);
        }
        if doc.downgrades.get().is_none() {
            let mut history = Vec::new();
            for (version, raw) in &doc.versions {
                let Some(date) = doc
                    .time
                    .get(version)
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<Timestamp>().ok())
                else {
                    continue;
                };
                let Ok(parsed) = version.parse::<Version>() else {
                    continue;
                };
                history.push((date, parsed, version, raw.rank()?));
            }
            history.sort_unstable();
            let (mut all, mut stable) = (0, 0);
            let mut downgrades = BTreeSet::new();
            // Releases published at the same instant are compared only with
            // strictly earlier publications.
            for group in history.chunk_by(|a, b| a.0 == b.0) {
                let (mut group_all, mut group_stable) = (all, stable);
                for (_, semver, version, rank) in group {
                    let is_stable = semver.pre_release().is_empty();
                    if *rank < if is_stable { stable } else { all } {
                        downgrades.insert((*version).clone());
                    }
                    group_all = group_all.max(*rank);
                    if is_stable {
                        group_stable = group_stable.max(*rank);
                    }
                }
                (all, stable) = (group_all, group_stable);
            }
            let _ = doc.downgrades.set(downgrades);
        }
        Ok(!doc.downgrades.get().expect("computed history").contains(v))
    }
}
