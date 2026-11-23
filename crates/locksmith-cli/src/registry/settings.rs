use super::{credential, extend};
use crate::Result;
use serde_json::Value;
use std::{collections::BTreeMap, env};

pub(super) fn env_config(prefix: &str, key: &str) -> Option<String> {
    let name = format!("{prefix}_config_{key}");
    env::var(&name)
        .or_else(|_| env::var(name.to_ascii_uppercase()))
        .ok()
}

pub(super) fn copy_settings(settings: &mut Value, from: &Value) {
    for key in [
        "registry",
        "registries",
        "registriesByScope",
        "namedRegistries",
        "networkConcurrency",
        "minimumReleaseAge",
        "minimumReleaseAgeStrict",
        "minimumReleaseAgeExclude",
        "minimumReleaseAgeIgnoreMissingTime",
        "resolutionMode",
        "trustPolicy",
        "trustLockfile",
        "trustPolicyExclude",
        "trustPolicyIgnoreAfter",
        "lockfileIncludeTarballUrl",
        "pnprServer",
    ] {
        if let Some(value) = from.get(key) {
            if key == "registries"
                && let Some(current) = settings[key].as_object_mut()
                && let Some(extra) = value.as_object()
            {
                current.extend(extra.clone());
            } else {
                settings[key] = value.clone();
            }
        }
    }
}

pub(super) fn project_rc(settings: &mut BTreeMap<String, String>, text: &str, credentials: bool) {
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // A checked-in file may contain literal credentials. Only user-owned
        // config may interpolate a secret from the process environment.
        let supported = key == "registry"
            || key.ends_with(":registry")
            || matches!(
                key,
                "minimum-release-age"
                    | "network-concurrency"
                    | "minimum-release-age-strict"
                    | "trust-policy"
                    | "resolution-mode"
                    | "lockfile-include-tarball-url"
            );
        if supported || (credentials && credential(key) && !value.contains("${")) {
            extend(settings, line, |_| true);
        }
    }
}

pub(super) fn apply_auth(
    rc: &mut BTreeMap<String, String>,
    auth: &Value,
) -> Result<BTreeMap<String, String>> {
    let mut routes = BTreeMap::new();
    for (url, scopes) in auth
        .as_object()
        .ok_or("_auth must map registries to scopes")?
    {
        let (_, host) = url
            .split_once("://")
            .ok_or("invalid registry URL in _auth")?;
        let root = format!("//{}/", host.trim_end_matches('/'));
        for (scope, entry) in scopes
            .as_object()
            .ok_or("invalid registry scopes in _auth")?
        {
            let route = if scope == "@" { "default" } else { scope };
            if routes.insert(route.to_owned(), url.clone()).is_some() {
                return Err(format!("multiple registry auth routes for {scope}").into());
            }
            if let Some(token) = entry["authToken"].as_str() {
                let key = if scope == "@" {
                    format!("{root}:_authToken")
                } else {
                    format!("{root}:{scope}:_authToken")
                };
                rc.insert(key, token.to_owned());
            }
        }
    }
    Ok(routes)
}
