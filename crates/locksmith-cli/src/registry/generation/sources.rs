//! Read the package manifest from the selected source without extracting a package tree.
use std::{
    fs,
    io::{self, Read},
    path::{Component, Path},
    process::Command,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use fluent_uri::{Uri, UriRef};
use locksmith::SourceManifest;
use ring::digest;
use serde_json::{Value, json};

use super::GenerationSource;
use crate::{Result, workspace};

pub(super) fn resolve(
    source: &mut GenerationSource<'_>,
    from: &str,
    name: &str,
    spec: &str,
) -> Result<SourceManifest> {
    let path = spec
        .strip_prefix("file:")
        .or_else(|| spec.starts_with(['.', '/']).then_some(spec));
    if let Some(path) = path {
        let path = Path::new(from).join(path).canonicalize()?;
        // Both sides use the canonical form, which on Windows carries the verbatim prefix.
        let relative = relative(&std::env::current_dir()?.canonicalize()?, &path);
        let locator = format!("file:{relative}");
        if path.is_dir() {
            let raw = workspace::read_manifest(&relative, workspace::working_file)?
                .ok_or_else(|| format!("missing package manifest in {}", path.display()))?;
            return package(
                name,
                &locator,
                raw,
                json!({"directory":relative,"type":"directory"}),
            );
        }
        return archive(name, &locator, fs::File::open(path)?, None);
    }
    if git_spec(spec) {
        return git(name, spec, source.network);
    }
    if spec.starts_with("https://") || spec.starts_with("http://") {
        return remote_archive(source, name, spec, None);
    }
    Err(format!("source manifest and resolution required for {spec}").into())
}

pub(super) fn locked(
    source: &mut GenerationSource<'_>,
    name: &str,
    locator: &str,
    resolution: &Value,
) -> Result<SourceManifest> {
    if resolution["type"] != "git"
        && (locator.starts_with("https://") || locator.starts_with("http://"))
    {
        remote_archive(source, name, locator, resolution["integrity"].as_str())
    } else {
        resolve(source, ".", name, locator)
    }
}

fn remote_archive(
    source: &mut GenerationSource<'_>,
    name: &str,
    spec: &str,
    expected_integrity: Option<&str>,
) -> Result<SourceManifest> {
    let network = source.network;
    let cache_manifest = source.hook_manifests;
    let client = source.client()?;
    let auth = client.auth(spec, name)?;
    let cache = |integrity: &str| {
        super::cache_file(
            spec,
            &format!("tarball-manifest/{integrity}"),
            auth.as_deref(),
        )
    };
    if let Some(integrity) = expected_integrity
        && let Some(path) = cache(integrity)
    {
        match fs::File::open(path) {
            Ok(file) => {
                return package(
                    name,
                    spec,
                    serde_json::from_reader(io::BufReader::new(file))?,
                    json!({"integrity":integrity,"tarball":spec}),
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    if !network {
        return Err(format!("missing tarball manifest for {name}; use --allow-network").into());
    }
    let release = download_archive(client, name, spec, expected_integrity)?;
    let integrity = release.resolution["integrity"]
        .as_str()
        .expect("archive integrity");
    if cache_manifest && let Some(path) = cache(integrity) {
        crate::registry::metadata::store_json(&path, &release.manifest)?;
    }
    Ok(release)
}

fn download_archive(
    client: &super::Registry,
    name: &str,
    spec: &str,
    integrity: Option<&str>,
) -> Result<SourceManifest> {
    // Tarball hosts commonly redirect to a CDN. Follow a bounded chain and
    // send registry credentials only to the original origin.
    let original: ureq::http::Uri = spec.parse()?;
    let mut url = original.clone();
    for _ in 0..10 {
        let text = url.to_string();
        let mut request = client.agent.get(&text);
        if same_origin(&url, &original)
            && let Some(auth) = client.auth(&text, name)?
        {
            request = request.header("Authorization", &auth);
        }
        let mut response = request.call()?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("tarball redirect without a location")?;
            let base = Uri::parse(text.as_str())?;
            url = UriRef::parse(location)?
                .resolve_against(&base)?
                .strip_fragment()
                .as_str()
                .parse()?;
            if !matches!(url.scheme_str(), Some("http" | "https"))
                || url.authority().is_none_or(|a| a.as_str().contains('@'))
            {
                return Err("expected an HTTP(S) tarball redirect without credentials".into());
            }
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("tarball source returned HTTP {}", response.status()).into());
        }
        return archive(name, spec, response.body_mut().as_reader(), integrity);
    }
    Err(format!("too many redirects for tarball {spec}").into())
}

fn same_origin(left: &ureq::http::Uri, right: &ureq::http::Uri) -> bool {
    let port = |uri: &ureq::http::Uri| {
        uri.port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            })
    };
    left.scheme() == right.scheme()
        && left
            .host()
            .zip(right.host())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        && port(left) == port(right)
}

fn package(fallback: &str, locator: &str, raw: Value, resolution: Value) -> Result<SourceManifest> {
    let actual = raw["name"].as_str().unwrap_or(fallback).to_owned();
    Ok(SourceManifest::new(
        format!("{actual}@{locator}"),
        raw,
        resolution,
    )?)
}

fn relative(from: &Path, to: &Path) -> String {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out: std::path::PathBuf = (common..a.len()).map(|_| "..").collect();
    out.extend(b[common..].iter().map(|c| c.as_os_str()));
    out.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn archive(
    name: &str,
    locator: &str,
    mut reader: impl Read,
    expected: Option<&str>,
) -> Result<SourceManifest> {
    // Hash compressed bytes, including data after the manifest and tar end markers.
    let mut compressed = tempfile::tempfile()?;
    let (algorithm, digests) = match expected {
        Some(value) => locked_integrity(value)?,
        None => (&digest::SHA512, Vec::new()),
    };
    let mut hash = digest::Context::new(algorithm);
    let mut buf = [0u8; 64 * 1024];
    loop {
        use io::Write;
        let count = reader.read(&mut buf)?;
        if count == 0 {
            break;
        }
        hash.update(&buf[..count]);
        compressed.write_all(&buf[..count])?;
    }
    let hash = STANDARD.encode(hash.finish());
    let integrity = match expected {
        Some(value)
            if digests
                .iter()
                .any(|&digest| digest == hash || digest == hash.trim_end_matches('=')) =>
        {
            value.to_owned()
        }
        Some(_) => {
            return Err(
                format!("tarball integrity differs from the locked release for {name}").into(),
            );
        }
        None => format!("sha512-{hash}"),
    };
    io::Seek::rewind(&mut compressed)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(compressed));
    for entry in tar.entries()? {
        let entry = entry?;
        if entry.header().entry_type().is_file() {
            let path = entry.path()?;
            let parts: Vec<_> = path.components().collect();
            if matches!(&parts[..],[Component::Normal(_),Component::Normal(n)] | [Component::Normal(n)] if *n=="package.json")
            {
                let raw: Value = serde_json::from_reader(entry)?;
                return package(
                    name,
                    locator,
                    raw,
                    json!({"integrity":integrity,"tarball":locator}),
                );
            }
        }
    }
    Err(format!("no package.json in {locator}").into())
}

fn locked_integrity(value: &str) -> Result<(&'static digest::Algorithm, Vec<&str>)> {
    // SRI accepts any matching digest of the strongest algorithm present.
    for (name, algorithm) in [
        ("sha512", &digest::SHA512),
        ("sha384", &digest::SHA384),
        ("sha256", &digest::SHA256),
        ("sha1", &digest::SHA1_FOR_LEGACY_USE_ONLY),
    ] {
        let hashes: Vec<_> = value
            .split_ascii_whitespace()
            .filter_map(|token| token.split_once('-'))
            .filter(|(prefix, _)| *prefix == name)
            .map(|(_, hash)| hash.split_once('?').map_or(hash, |(digest, _)| digest))
            .collect();
        if !hashes.is_empty() {
            return Ok((algorithm, hashes));
        }
    }
    Err("missing supported tarball integrity digest".into())
}

fn git_spec(spec: &str) -> bool {
    spec.starts_with("git+")
        || spec.starts_with("git://")
        || spec.starts_with("ssh://")
        || spec.starts_with("github:")
        || spec.split('#').next().is_some_and(|url| {
            url.ends_with(".git") || (!url.contains(':') && url.split('/').count() == 2)
        })
}

fn git(name: &str, spec: &str, network: bool) -> Result<SourceManifest> {
    let (remote, fragment) = spec.split_once('#').unwrap_or((spec, ""));
    let https;
    let url = if let Some(repo) = remote
        .strip_prefix("github:")
        .or_else(|| (!remote.contains(':')).then_some(remote))
    {
        https = format!("https://github.com/{}.git", repo.trim_end_matches(".git"));
        &https
    } else {
        remote.strip_prefix("git+").unwrap_or(remote)
    };
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s)
        .ok_or("expected a Git URL")?;
    if !matches!(scheme, "http" | "https" | "ssh" | "git" | "file") {
        return Err(format!("unsupported Git transport {scheme}").into());
    }
    let mut revision = "HEAD";
    let mut subdir = "";
    for part in fragment.split('&').filter(|s| !s.is_empty()) {
        if let Some(path) = part.strip_prefix("path:") {
            subdir = path.trim_matches('/');
        } else {
            revision = part;
        }
    }
    if revision.starts_with('-') || subdir.split('/').any(|p| p == "..") {
        return Err("invalid Git revision or package directory".into());
    }
    let cache = |commit: &str| {
        super::cache_file(
            &format!("{url}#{commit}&path:/{subdir}"),
            "git-package-manifest",
            None,
        )
    };
    let finish = |commit: &str, raw| {
        let mut locator = format!("git+{url}#{commit}");
        let mut resolution = json!({"type":"git","repo":url,"commit":commit});
        if !subdir.is_empty() {
            resolution["path"] = format!("/{subdir}").into();
            locator.push_str(&format!("&path:/{subdir}"));
        }
        package(name, &locator, raw, resolution)
    };
    if revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        let commit = revision.to_ascii_lowercase();
        if let Some(path) = cache(&commit) {
            match fs::File::open(path) {
                Ok(file) => {
                    return finish(&commit, serde_json::from_reader(io::BufReader::new(file))?);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    if scheme != "file" && !network {
        return Err(format!("missing Git manifest for {name}; use --allow-network").into());
    }
    let temp = tempfile::tempdir()?;
    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .args([
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(temp.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        if !out.status.success() {
            return Err(format!(
                "Git source {name}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into());
        }
        Ok(String::from_utf8(out.stdout)?)
    };
    let tag;
    if let Some(range) = revision.strip_prefix("semver:") {
        let range = range.parse::<node_semver::Range>()?;
        let refs = run(&["ls-remote", "--refs", "--tags", "--", url])?;
        tag = refs
            .lines()
            .filter_map(|line| {
                let (_, tag) = line.split_once("\trefs/tags/")?;
                let v = tag
                    .trim_start_matches('v')
                    .parse::<node_semver::Version>()
                    .ok()?;
                v.satisfies(&range).then_some((v, tag))
            })
            .max()
            .ok_or("no Git tag satisfies the semver selector")?
            .1
            .to_owned();
        revision = &tag;
    }
    run(&["init", "--bare", "--quiet"])?;
    run(&["remote", "add", "origin", url])?;
    run(&["config", "remote.origin.promisor", "true"])?;
    run(&["config", "remote.origin.partialclonefilter", "blob:none"])?;
    run(&[
        "fetch",
        "--quiet",
        "--no-tags",
        "--depth=1",
        "--filter=blob:none",
        "--",
        "origin",
        revision,
    ])?;
    let commit = run(&["rev-parse", "FETCH_HEAD^{commit}"])?;
    let commit = commit.trim();
    let path = if subdir.is_empty() {
        "package.json".to_owned()
    } else {
        format!("{subdir}/package.json")
    };
    let raw = serde_json::from_str(&run(&["show", &format!("{commit}:{path}")])?)?;
    let manifest = finish(commit, raw)?;
    if let Some(path) = cache(commit) {
        crate::registry::metadata::store_json(&path, &manifest.manifest)?;
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn a_locked_archive_is_verified_with_its_recorded_hash() {
        let raw = br#"{"name":"package","version":"1.0.0"}"#;
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(raw.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "package/package.json", raw.as_slice())
            .unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        let url = "https://mirror.example.invalid/package.tgz";
        let sri = format!("sha256-{}", STANDARD.encode(Sha256::digest(&bytes)));
        let release = archive("package", url, bytes.as_slice(), Some(&sri)).unwrap();
        assert_eq!(release.resolution["integrity"], sri);
        let bad = format!(
            "sha256-{}",
            STANDARD.encode(Sha256::digest(b"other archive"))
        );
        assert!(
            archive("package", url, bytes.as_slice(), Some(&bad))
                .unwrap_err()
                .to_string()
                .contains("tarball integrity differs")
        );
        let stronger = format!(
            "{sri} sha512-{}",
            STANDARD.encode(digest::digest(&digest::SHA512, b"other archive"))
        );
        assert!(archive("package", url, bytes.as_slice(), Some(&stronger)).is_err());
    }
}
