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
    let mut files: Vec<_> = files.iter().collect();
    files.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
    if !hash.starts_with("sha256-") {
        return files.len() == 1
            && files[0].1.as_str().is_some_and(|s| {
                crate::peer::patch_hash(s, crate::PatchEncoding::Md5Base32WithPath) == hash
            });
    }
    let digest = |s: &str| {
        format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(s.as_bytes()))
        )
    };
    let Some(hashes) = files
        .into_iter()
        .map(|(_, v)| Some(digest(&v.as_str()?.replace("\r\n", "\n"))))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    match hashes.as_slice() {
        [only] => only == hash,
        many => digest(&many.join(",")) == hash,
    }
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
        format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(text.as_bytes()))
        )
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
