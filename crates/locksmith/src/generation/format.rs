//! Version-specific representations written by pnpm.
use crate::Value;

/// pnpm annotates immutable Git-hosted tarballs when importing older locks.
pub(super) fn mark_git_archives(doc: &mut Value, version: Option<&node_semver::Version>) {
    use node_semver::Version;
    let supported = version.is_some_and(|v| match v.major() {
        10 => v >= &Version::from((10, 33, 4)),
        11 => v >= &Version::from((11, 1, 0)),
        major => major >= 12,
    });
    if !supported {
        return;
    }
    for record in doc
        .get_mut("packages")
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|m| m.values_mut())
    {
        let resolution = &mut record["resolution"];
        if resolution.get("type").is_none()
            && resolution.get("gitHosted").is_none()
            && resolution["tarball"]
                .as_str()
                .is_some_and(immutable_git_archive)
        {
            resolution["gitHosted"] = true.into();
        }
    }
}

fn immutable_git_archive(tarball: &str) -> bool {
    let Ok(url) = tarball.parse::<http::Uri>() else {
        return false;
    };
    if url.scheme() != Some(&http::uri::Scheme::HTTPS) {
        return false;
    }
    let Some(host) = url.host() else { return false };
    let path: Vec<_> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let commit = |s: &str| s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit());
    match (host.to_ascii_lowercase().as_str(), path.as_slice()) {
        ("codeload.github.com", [_, _, "tar.gz", id]) => commit(id),
        ("bitbucket.org", [_, _, "get", file]) => file.strip_suffix(".tar.gz").is_some_and(commit),
        ("gitlab.com", ["api", "v4", "projects", _, "repository", "archive.tar.gz"]) => url
            .query()
            .into_iter()
            .flat_map(|s| s.split('&'))
            .find_map(|s| s.strip_prefix("ref="))
            .is_some_and(commit),
        ("gitlab.com", [_, _, .., "-", "archive", id, file]) => {
            commit(id) && file.ends_with(".tar.gz")
        }
        _ => false,
    }
}
