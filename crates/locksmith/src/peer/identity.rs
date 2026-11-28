//! Encode and decode pnpm peer identities, and linked paths.
use super::{WorkspaceLinks, canonical_binding};
use crate::{
    LinkedPeerEncoding,
    model::checksum::{HashStyle, digest},
    model::graph,
    model::pin,
};
use std::collections::BTreeMap;

/// Parse balanced suffixes without splitting nested peer identities. Hash-only
/// suffixes deliberately remain opaque.
pub(crate) fn context(key: &str) -> Option<BTreeMap<&str, &str>> {
    let mut result = BTreeMap::new();
    for (name, value) in context_tokens(key)? {
        if result.insert(name, value).is_some() {
            return None;
        }
    }
    Some(result)
}

pub(super) fn context_tokens(key: &str) -> Option<Vec<(&str, &str)>> {
    let mut result = Vec::new();
    let mut suffix = &key[pin::package_key(key).len()..];
    while !suffix.is_empty() {
        let mut depth = 0;
        let mut end = None;
        for (i, c) in suffix.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        let token = suffix.get(1..end)?;
        let (name, value) = if let Some(hash) = token.strip_prefix("patch_hash=") {
            ("patch_hash", hash)
        } else {
            let head = token.split('(').next()?;
            let start = if head.starts_with('@') {
                head.find('/')? + 1
            } else {
                0
            };
            let at = start + head[start..].find('@')?;
            (&token[..at], &token[at + 1..])
        };
        result.push((name, value));
        suffix = &suffix[end + 1..];
    }
    Some(result)
}

pub(super) fn peer_text(
    peers: &BTreeMap<String, String>,
    flat: bool,
    linked_peer_encoding: LinkedPeerEncoding,
) -> String {
    let mut ids = peers
        .iter()
        .filter(|(n, _)| n.as_str() != "patch_hash")
        .map(|(n, v)| peer_id(n, v, flat, linked_peer_encoding))
        .collect::<Vec<_>>();
    // pnpm sorts complete identities: `react-dom@…` precedes `react@…`.
    ids.sort_unstable();
    ids.join(")(")
}

pub(super) fn peer_id(name: &str, value: &str, flat: bool, encoding: LinkedPeerEncoding) -> String {
    if let Some(path) = value.strip_prefix("link:") {
        return format!("{name}@{}", link_peer_id(path, encoding));
    }
    let (name, value) = canonical_binding(name, value);
    format!(
        "{name}@{}",
        if flat { pin::package_key(value) } else { value }
    )
}

pub(crate) fn root_provider(importer: &str, reference: &str) -> String {
    reference.strip_prefix("link:").map_or_else(
        || reference.into(),
        |path| format!("link:{}", graph::linked_importer(importer, path)),
    )
}

// Version-specific path encoding belongs to the instance identifier. The
// dependency edge retains the original root-relative workspace link.
pub(super) fn link_peer_id(path: &str, encoding: LinkedPeerEncoding) -> String {
    if encoding == LinkedPeerEncoding::Path {
        let mut result = String::with_capacity(path.len());
        let mut last_plus = true;
        for ch in path.trim_start_matches('.').chars() {
            if ch < ' ' || "\"*+/:<>?\\|".contains(ch) {
                if !last_plus {
                    result.push('+');
                }
                last_plus = true;
            } else {
                result.push(ch);
                last_plus = false;
            }
        }
        let len = result.trim_end_matches(['+', '.']).len();
        result.truncate(len);
        if result.is_empty() && !path.is_empty() {
            result.push('+');
        }
        return result;
    }
    let mut encoded = String::with_capacity(path.len().min(100));
    for ch in path.chars() {
        let ch = if ch <= '\u{1f}'
            || ('\u{80}'..='\u{9f}').contains(&ch)
            || "<>:\"/\\|?*".contains(ch)
        {
            '+'
        } else {
            ch
        };
        if ch != '+' || !encoded.ends_with('+') {
            encoded.push(ch);
        }
    }
    if encoded.starts_with('.') {
        encoded = format!("+{}", encoded.trim_start_matches('.'));
    }
    let encoded = encoded.trim_end_matches('.');
    let encoded = if !encoded.is_empty() && encoded.chars().all(|ch| ch == '+') {
        "+"
    } else if encoded.len() > 1 {
        encoded.trim_matches('+')
    } else {
        encoded
    };
    let lower = encoded.to_ascii_lowercase();
    let reserved = matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || (lower.len() == 4
            && (lower.starts_with("com") || lower.starts_with("lpt"))
            && lower.as_bytes()[3].is_ascii_digit());
    let mut units: Vec<_> = encoded.encode_utf16().take(100).collect();
    if reserved && units.len() < 100 {
        units.push(u16::from(b'+'));
    }
    String::from_utf16_lossy(&units)
}

pub(super) fn token_matches(name: &str, reference: &str, token_name: &str, token: &str) -> bool {
    if let Some(path) = reference.strip_prefix("link:") {
        return name == token_name
            && [LinkedPeerEncoding::Filename, LinkedPeerEncoding::Path]
                .into_iter()
                .any(|encoding| link_peer_id(path, encoding) == token);
    }
    let (actual, reference) = canonical_binding(name, reference);
    actual == token_name && (reference == token || pin::package_key(reference) == token)
}

pub(super) fn token_reference(links: &WorkspaceLinks, name: &str, token: &str) -> Option<String> {
    links
        .get(name)
        .and_then(|values| values.get(token))
        .cloned()
        .unwrap_or_else(|| Some(token.into()))
}
/// Verify a projected provider map against an identity already written by pnpm.
/// A new peer can move an instance across pnpm's suffix length threshold.
pub(crate) fn matches_bindings(
    key: &str,
    bindings: &BTreeMap<String, String>,
    flat: bool,
    encoding: LinkedPeerEncoding,
) -> bool {
    let suffix = key
        .rsplit_once('(')
        .and_then(|(_, tail)| tail.strip_suffix(')'));
    let style = suffix.and_then(HashStyle::recognize);
    encode(pin::package_key(key), bindings, style, flat, encoding) == key
}

/// Encode a mix of two witnessed contexts only if the same pnpm encoding
/// reproduces both input identities, including the suffix length threshold.
pub(crate) fn projected_identity(
    keys: [&str; 2],
    evidence: [&BTreeMap<String, String>; 2],
    target: &BTreeMap<String, String>,
    config: &crate::WorkspaceConfig,
    lock: &crate::Value,
) -> Option<String> {
    let observed = keys
        .iter()
        .filter_map(|k| HashStyle::recognize(k.rsplit_once('(')?.1.strip_suffix(')')?))
        .next();
    let hash = observed.or(config.extension_checksum.map(|h| match h {
        crate::ExtensionChecksum::Md5 => HashStyle::Md5Base32,
        crate::ExtensionChecksum::Sha256 => HashStyle::Sha256,
    }));
    // pnpm persists non-default suffix limits in lockfile settings.
    let limit = lock["settings"]
        .get("peersSuffixMaxLength")
        .map_or(Some(1000), |v| v.as_u64())?;
    let package = pin::package_key(keys[0]);
    let identity = |pins: &BTreeMap<String, String>| {
        let flat = config.resolution.lock.dedupe_peers;
        let encoding = config.linked_peer_encoding;
        let long = peer_text(pins, flat, encoding).encode_utf16().count() as u64 > limit;
        Some(encode(
            package,
            pins,
            if long { Some(hash?) } else { None },
            flat,
            encoding,
        ))
    };
    for (key, pins) in keys.into_iter().zip(evidence) {
        if identity(pins)? != key {
            return None;
        }
    }
    identity(target)
}

pub(super) fn encode(
    package: &str,
    peers: &BTreeMap<String, String>,
    hash: Option<HashStyle>,
    flat: bool,
    linked_peer_encoding: LinkedPeerEncoding,
) -> String {
    let mut key = package.to_owned();
    if let Some(patch) = peers.get("patch_hash") {
        key.push_str(&format!("(patch_hash={patch})"));
    }
    let text = peer_text(peers, flat, linked_peer_encoding);
    if !text.is_empty() {
        key.push('(');
        match hash {
            Some(style) => key.push_str(&digest(&text, style)),
            None => key.push_str(&text),
        };
        key.push(')');
    }
    key
}
