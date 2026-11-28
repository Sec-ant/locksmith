//! Checksums for pnpm configuration, patches, and peer identities.
use crate::Value;
use base64::Engine;
use sha2::Digest;
use std::fmt::Write;

/// Select the encoding from a source checksum and verify its configuration
/// before encoding a newly composed configuration in the same format.
fn is_legacy_checksum(checksum: &str) -> Option<bool> {
    if checksum.starts_with("sha256-") {
        Some(false)
    } else if checksum.len() == 32 && checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(true)
    } else {
        None
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn extensions(
    value: &Value,
    source: &Value,
    checksum: &str,
    target: Option<crate::ExtensionChecksum>,
) -> Option<String> {
    let legacy = is_legacy_checksum(checksum)?;
    (calculate(source, legacy) == checksum).then(|| {
        calculate(
            value,
            target.map_or(legacy, |v| v == crate::ExtensionChecksum::Md5),
        )
    })
}

pub(crate) fn extensions_match(config: &crate::WorkspaceConfig, recorded: Option<&Value>) -> bool {
    let configured = config
        .resolution
        .package_extensions
        .as_ref()
        .filter(|value| {
            !value.is_null() && value.as_object().is_none_or(|rules| !rules.is_empty())
        });
    match (configured, recorded) {
        (None, None) => true,
        (Some(rules), Some(Value::String(hash))) => {
            let Some(legacy) = is_legacy_checksum(hash) else {
                return false;
            };
            let target_legacy = config
                .extension_checksum
                .map_or(legacy, |v| v == crate::ExtensionChecksum::Md5);
            target_legacy == legacy && calculate(rules, legacy) == *hash
        }
        _ => false,
    }
}

/// The CLI supplies hook contents keyed by path. A pure caller may instead
/// supply a checksum it already verified.
pub(crate) fn hooks_match(input: Option<&Value>, recorded: Option<&Value>) -> bool {
    let (Some(input), Some(recorded)) = (input, recorded) else {
        return input.is_none() && recorded.is_none();
    };
    if input.is_string() {
        return input == recorded;
    }
    let (Some(files), Some(hash)) = (input.as_object(), recorded.as_str()) else {
        return false;
    };
    if !hash.starts_with("sha256-") {
        return files.len() == 1
            && files
                .values()
                .next()
                .and_then(Value::as_str)
                .is_some_and(|content| {
                    patch_hash(content, crate::PatchEncoding::Md5Base32WithPath) == hash
                });
    }
    let mut hashes = Vec::with_capacity(files.len());
    for (path, content) in files {
        let Some(content) = content.as_str() else {
            return false;
        };
        hashes.push((path, sha256(&content.replace("\r\n", "\n"))));
    }
    match hashes.as_slice() {
        // pnpm 12 records an empty-list digest when only a global hook is active.
        [] => return sha256("") == hash,
        [(_, digest)] => return digest == hash,
        _ => {}
    }
    // Older pnpm sorts paths as JavaScript strings. pnpm 12 uses Rust Path order.
    let combined = |path_order| {
        let mut ordered: Vec<_> = hashes.iter().collect();
        if path_order {
            ordered.sort_by(|(a, _), (b, _)| std::path::Path::new(a).cmp(std::path::Path::new(b)));
        } else {
            ordered.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
        }
        let parts: Vec<_> = ordered.iter().map(|(_, digest)| digest.as_str()).collect();
        sha256(&parts.join(","))
    };
    combined(false) == hash || combined(true) == hash
}

fn sha256(text: &str) -> String {
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(text.as_bytes()))
    )
}

fn calculate(value: &Value, legacy: bool) -> String {
    let mut text = String::new();
    if legacy {
        json(value, &mut text);
        hex(&md5::Md5::digest(text.as_bytes()))
    } else {
        object_hash(value, &mut text);
        sha256(&text)
    }
}

fn string(value: &str, out: &mut String) {
    write!(out, "string:{}:{value}", value.encode_utf16().count()).unwrap();
}

fn number(value: &serde_json::Number, out: &mut String) {
    out.push_str(ryu_js::Buffer::new().format_finite(value.as_f64().expect("finite JSON number")));
}

// object-hash 3 with respectType=false and unordered objects, arrays, and sets.
// JSON values have no shared object references or cycles.
fn object_hash(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("Null"),
        Value::Bool(value) => write!(out, "bool:{value}").unwrap(),
        Value::Number(value) => {
            out.push_str("number:");
            number(value, out);
        }
        Value::String(value) => string(value, out),
        Value::Array(values) => {
            write!(out, "array:{}:", values.len()).unwrap();
            if values.len() <= 1 {
                for value in values {
                    object_hash(value, out);
                }
            } else {
                let mut entries: Vec<_> = values
                    .iter()
                    .map(|value| {
                        let mut text = String::new();
                        object_hash(value, &mut text);
                        text
                    })
                    .collect();
                entries.sort_unstable_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                write!(out, "array:{}:", entries.len()).unwrap();
                for entry in entries {
                    string(&entry, out);
                }
            }
        }
        Value::Object(values) => {
            write!(out, "object:{}:", values.len()).unwrap();
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            for (key, value) in entries {
                string(key, out);
                out.push(':');
                object_hash(value, out);
                out.push(',');
            }
        }
    }
}

// JSON.stringify(sortKeys(value, { deep: true })), including JavaScript's
// enumeration of array-index object properties before other string keys.
fn json(value: &Value, out: &mut String) {
    match value {
        Value::Number(value) => number(value, out),
        Value::Array(values) => {
            out.push('[');
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json(value, out);
            }
            out.push(']');
        }
        Value::Object(values) => {
            let index = |key: &str| {
                key.parse::<u32>()
                    .ok()
                    .filter(|n| *n < u32::MAX && n.to_string() == key)
            };
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|(a, _), (b, _)| match (index(a), index(b)) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.encode_utf16().cmp(b.encode_utf16()),
            });
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("JSON string"));
                out.push(':');
                json(value, out);
            }
            out.push('}');
        }
        _ => out.push_str(&value.to_string()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HashStyle {
    Sha256,
    Md5Base32,
}
impl HashStyle {
    pub(crate) fn recognize(hash: &str) -> Option<Self> {
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
pub(crate) fn digest(text: &str, style: HashStyle) -> String {
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
            hex(&sha2::Sha256::digest(normalized.as_bytes()))
        }
    }
}
