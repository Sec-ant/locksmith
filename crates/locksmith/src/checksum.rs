//! pnpm's configuration checksum encodings, verified against supplied records.
use crate::Value;
use base64::Engine;
use sha2::Digest;
use std::fmt::Write;

/// Select the encoding from a source checksum and verify its configuration
/// before encoding a newly composed configuration in the same format.
pub(crate) fn extensions(
    value: &Value,
    source: &Value,
    checksum: &str,
    target: Option<crate::ExtensionChecksum>,
) -> Option<String> {
    let legacy = if checksum.starts_with("sha256-") {
        false
    } else if checksum.len() == 32 && checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
        true
    } else {
        return None;
    };
    (calculate(source, legacy) == checksum).then(|| {
        calculate(
            value,
            target.map_or(legacy, |v| v == crate::ExtensionChecksum::Md5),
        )
    })
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
                    crate::peer::patch_hash(content, crate::PatchEncoding::Md5Base32WithPath)
                        == hash
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
        md5::Md5::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
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
