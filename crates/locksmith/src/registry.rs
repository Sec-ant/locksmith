//! Published manifests supplied by the caller. Resolution itself performs no I/O.
use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;

use crate::{LockfileError, Value, manifest};

/// An input missing from a merge or convergence. A declaration lookup is for a release
/// already identified by an input lock, even when it is no longer a candidate.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegistryRequest {
    Version { name: String, selector: String },
    Manifest { name: String, version: String },
}

impl RegistryRequest {
    pub fn name(&self) -> &str {
        match self {
            Self::Version { name, .. } | Self::Manifest { name, .. } => name,
        }
    }

    pub fn selector(&self) -> &str {
        match self {
            Self::Version { selector, .. } => selector,
            Self::Manifest { version, .. } => version,
        }
    }

    pub(crate) fn version(name: &str, specifier: &str) -> Option<Self> {
        let (name, selector) = crate::reconcile::alias(specifier).unwrap_or((name, specifier));
        (valid_name(name) && (selector.parse::<node_semver::Range>().is_ok() || tag(selector)))
            .then(|| Self::Version {
                name: name.into(),
                selector: selector.into(),
            })
    }

    pub(crate) fn manifest(key: &str) -> Option<Self> {
        let (name, version) = crate::graph::package_key(key).rsplit_once('@')?;
        (valid_name(name) && version.parse::<node_semver::Version>().is_ok()).then(|| {
            Self::Manifest {
                name: name.into(),
                version: version.into(),
            }
        })
    }
}

fn valid_name(name: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && !matches!(s, "." | "..")
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._~-".contains(&c))
    };
    match name.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(s, n)| part(s) && part(n)),
        None => part(name),
    }
}

fn tag(selector: &str) -> bool {
    selector.starts_with(|c: char| c.is_ascii_alphabetic())
        && selector
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

#[derive(Debug, Default)]
pub struct RegistryMetadata {
    pub(crate) unavailable_manifests: BTreeSet<String>,
    pub(crate) releases: BTreeMap<String, Publication>,
    pub(crate) selections: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug)]
pub(crate) struct Publication {
    pub(crate) manifest: Value,
    pub(crate) resolution: Value,
}

impl RegistryMetadata {
    /// Record an unsuccessful original-manifest lookup during convergence.
    /// The installed edge remains usable, and other packages can still converge.
    pub fn skip_manifest(&mut self, name: &str, version: &str) {
        self.unavailable_manifests
            .insert(format!("{name}@{version}"));
    }

    pub(crate) fn selected(&self, name: &str, selector: &str) -> Option<&String> {
        self.selections.get(name)?.get(selector)
    }

    /// Supply the selected registry manifest and its registry base URL. The
    /// caller selects versions under its registry's tags, time and trust policy.
    pub fn insert(
        &mut self,
        request: &RegistryRequest,
        manifest: Value,
        registry: &str,
        include_tarball: bool,
    ) -> Result<(), LockfileError> {
        let name = request.name();
        let invalid = |reason: &str| {
            LockfileError::InvalidDocument(format!("registry metadata for {name}: {reason}"))
        };
        let version = manifest["version"]
            .as_str()
            .ok_or_else(|| invalid("missing version"))?;
        if !valid_name(name)
            || manifest["name"] != name
            || version.parse::<node_semver::Version>().is_err()
        {
            return Err(invalid("inconsistent package identity"));
        }
        let selector = request.selector();
        let valid = match request {
            RegistryRequest::Manifest { .. } => version == selector,
            RegistryRequest::Version { .. } => match selector.parse::<node_semver::Range>() {
                Ok(_) => selector == "*" || manifest::matches_version(selector, Some(version)),
                Err(_) => tag(selector),
            },
        };
        if !valid {
            return Err(invalid("selected version does not satisfy request"));
        }
        let dist = &manifest["dist"];
        let integrity = if let Some(value) = dist["integrity"].as_str() {
            if !value.split_ascii_whitespace().any(valid_digest) {
                return Err(invalid("invalid distribution integrity"));
            }
            value.to_owned()
        } else {
            let sha = dist["shasum"]
                .as_str()
                .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| invalid("missing distribution integrity"))?;
            let bytes: Vec<_> = (0..40)
                .step_by(2)
                .map(|i| u8::from_str_radix(&sha[i..i + 2], 16).expect("hex digest"))
                .collect();
            format!("sha1-{}", STANDARD.encode(bytes))
        };
        let mut resolution = json!({"integrity":integrity});
        let tarball = dist["tarball"]
            .as_str()
            .ok_or_else(|| invalid("missing distribution URL"))?;
        let authority = tarball
            .strip_prefix("https://")
            .or_else(|| tarball.strip_prefix("http://"))
            .and_then(|url| url.split('/').next());
        if authority.is_none_or(|host| host.is_empty() || host.contains(['@', '#', '?']))
            || tarball.chars().any(char::is_whitespace)
        {
            return Err(invalid("invalid HTTP(S) distribution URL"));
        }
        let bare = name.rsplit('/').next().expect("name");
        let expected = format!(
            "{}/{name}/-/{bare}-{version}.tgz",
            registry.trim_end_matches('/')
        );
        let normalized = tarball;
        if include_tarball
            || expected.split_once("://").map(|(_, url)| url)
                != normalized.split_once("://").map(|(_, url)| url)
        {
            resolution["tarball"] = Value::from(tarball);
        }
        let key = format!("{name}@{version}");
        if self
            .releases
            .get(&key)
            .is_some_and(|p| p.resolution != resolution || p.manifest != manifest)
        {
            return Err(invalid("inconsistent responses for the same release"));
        }
        if matches!(request, RegistryRequest::Version { .. }) {
            self.selections
                .entry(name.to_owned())
                .or_default()
                .insert(selector.to_owned(), key.clone());
        }
        self.releases.entry(key).or_insert(Publication {
            resolution,
            manifest,
        });
        Ok(())
    }
}

fn valid_digest(token: &str) -> bool {
    let Some((algorithm, digest)) = token.split_once('-') else {
        return false;
    };
    let size = match algorithm {
        "sha1" => 20,
        "sha256" => 32,
        "sha384" => 48,
        "sha512" => 64,
        _ => return false,
    };
    STANDARD
        .decode(digest.split('?').next().unwrap_or(digest))
        .is_ok_and(|bytes| bytes.len() == size)
}
