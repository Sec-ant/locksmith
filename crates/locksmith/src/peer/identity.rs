//! Encode and decode pnpm peer identities, linked paths and patch hashes.
use super::{WorkspaceLinks, canonical_binding};
use crate::{LinkedPeerEncoding, graph};
use sha2::Digest;
use std::collections::BTreeMap;

pub(crate) fn patch(key: &str) -> Option<&str> {
    key[graph::package_key(key).len()..]
        .strip_prefix("(patch_hash=")
        .and_then(|s| s.split_once(')').map(|(hash, _)| hash))
}

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
    let mut suffix = &key[graph::package_key(key).len()..];
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum HashStyle {
    Sha256,
    Md5Base32,
}
impl HashStyle {
    pub(super) fn recognize(hash: &str) -> Option<Self> {
        if hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(Self::Sha256)
        } else if hash.len() == 26
            && hash
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        {
            Some(Self::Md5Base32)
        } else {
            None
        }
    }
}
pub(super) fn digest(text: &str, style: HashStyle) -> String {
    match style {
        HashStyle::Sha256 => {
            let mut out = String::with_capacity(32);
            let hex = b"0123456789abcdef";
            for &b in &sha2::Sha256::digest(text.as_bytes())[..16] {
                out.push(hex[(b >> 4) as usize] as char);
                out.push(hex[(b & 15) as usize] as char);
            }
            out
        }
        HashStyle::Md5Base32 => {
            let bytes = md5::Md5::digest(text.as_bytes());
            let mut acc = 0u32;
            let mut bits = 0;
            let mut out = String::new();
            for b in bytes {
                acc = (acc << 8) | u32::from(b);
                bits += 8;
                while bits >= 5 {
                    bits -= 5;
                    out.push(
                        b"abcdefghijklmnopqrstuvwxyz234567"[((acc >> bits) & 31) as usize] as char,
                    );
                }
            }
            if bits > 0 {
                out.push(
                    b"abcdefghijklmnopqrstuvwxyz234567"[((acc << (5 - bits)) & 31) as usize]
                        as char,
                );
            }
            out
        }
    }
}

pub(crate) fn patch_hash(text: &str, encoding: crate::PatchEncoding) -> String {
    let normalized = text.replace("\r\n", "\n");
    match encoding {
        crate::PatchEncoding::Md5Base32WithPath => digest(&normalized, HashStyle::Md5Base32),
        crate::PatchEncoding::Sha256 | crate::PatchEncoding::Sha256WithPath => {
            sha2::Sha256::digest(normalized.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }
    }
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
        if flat {
            graph::package_key(value)
        } else {
            value
        }
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

pub(crate) fn token_matches(name: &str, reference: &str, token_name: &str, token: &str) -> bool {
    if let Some(path) = reference.strip_prefix("link:") {
        return name == token_name
            && [LinkedPeerEncoding::Filename, LinkedPeerEncoding::Path]
                .into_iter()
                .any(|encoding| link_peer_id(path, encoding) == token);
    }
    let (actual, reference) = canonical_binding(name, reference);
    actual == token_name && (reference == token || graph::package_key(reference) == token)
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
    encode(graph::package_key(key), bindings, style, flat, encoding) == key
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
    let package = graph::package_key(keys[0]);
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

pub(crate) fn generated_identity(
    package: &str,
    bindings: &BTreeMap<String, String>,
    config: &crate::WorkspaceConfig,
    limit: u64,
) -> String {
    let flat = config.resolution.lock.dedupe_peers;
    let encoding = config.linked_peer_encoding;
    let long = peer_text(bindings, flat, encoding).encode_utf16().count() as u64 > limit;
    let style = match config.extension_checksum {
        Some(crate::ExtensionChecksum::Md5) => HashStyle::Md5Base32,
        _ => HashStyle::Sha256,
    };
    encode(package, bindings, long.then_some(style), flat, encoding)
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
