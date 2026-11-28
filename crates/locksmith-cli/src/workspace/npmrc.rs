use serde_json::{Value, json};

use crate::Result;

// pnpm 9 and 10 also read these resolver and peer settings from project .npmrc.
pub(super) fn legacy_settings(
    text: &str,
    config: &mut Value,
    version: Option<&node_semver::Version>,
) -> Result<()> {
    const FLAGS: &[(&str, &str)] = &[
        ("auto-install-peers", "autoInstallPeers"),
        ("ignore-pnpmfile", "ignorePnpmfile"),
        ("dedupe-peers", "dedupePeers"),
        ("dedupe-peer-dependents", "dedupePeerDependents"),
        ("exclude-links-from-lockfile", "excludeLinksFromLockfile"),
        (
            "project-scoped-optional-peers",
            "projectScopedOptionalPeers",
        ),
        (
            "resolve-peers-from-workspace-root",
            "resolvePeersFromWorkspaceRoot",
        ),
        ("strict-peer-dependencies", "strictPeerDependencies"),
        ("inject-workspace-packages", "injectWorkspacePackages"),
        ("prefer-workspace-packages", "preferWorkspacePackages"),
    ];
    for line in text.lines().map(str::trim) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some((_, field)) = FLAGS.iter().find(|(flag, _)| key.trim() == *flag) {
            let flag = ini_value(value)
                .parse()
                .map_err(|_| format!("{key} expects true or false"))?;
            config[field] = Value::Bool(flag);
        } else {
            let unquoted = value.trim().trim_matches('"');
            match key.trim() {
                "link-workspace-packages" => {
                    config["linkWorkspacePackages"] = match unquoted {
                        "deep" => json!("deep"),
                        _ => json!(unquoted.parse::<bool>()?),
                    }
                }
                "peers-suffix-max-length"
                    if version.is_none_or(|v| v >= &node_semver::Version::from((9, 3, 0))) =>
                {
                    config["peersSuffixMaxLength"] = json!(ini_value(value).parse::<usize>()?)
                }
                "pnpmfile" => config["pnpmfile"] = json!(unquoted),
                _ => (),
            }
        }
    }
    Ok(())
}

fn ini_value(value: &str) -> &str {
    let value = value.trim();
    if let Some(quote @ ('"' | '\'')) = value.chars().next()
        && let Some((quoted, rest)) = value[1..].split_once(quote)
    {
        let rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with(['#', ';']) {
            return quoted;
        }
    }
    value
        .split_once(['#', ';'])
        .map_or(value, |(flag, _)| flag.trim_end())
}
