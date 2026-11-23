//! pnpm lockfile YAML formatting for fields whose source bytes have changed.
//! The scalar and collection writers are adapted from pnpm v12.14.0
//! (pnpm/crates/lockfile/src/yaml_emit). See yaml/LICENSE.
use serde_json::Value;
use std::cmp::Ordering;

/// Keys whose collection value always renders on a single line (flow style).
/// Mirrors the fork's [`SINGLE_LINE_KEYS`][fork-single-line-keys].
///
/// [fork-single-line-keys]: https://cdn.jsdelivr.net/npm/@zkochan/js-yaml@0.0.11/lib/dumper.js
const SINGLE_LINE_KEYS: [&str; 4] = ["cpu", "engines", "os", "libc"];

/// One indentation level, in spaces (`js-yaml`'s default `indent`).
const INDENT: usize = 2;

/// Per-package / per-snapshot key priority, matching pnpm's lockfile
/// key sort.
const ORDERED_KEYS: [&str; 20] = [
    "resolution",
    "id",
    "name",
    "version",
    "engines",
    "cpu",
    "os",
    "libc",
    "deprecated",
    "hasBin",
    "prepare",
    "requiresBuild",
    "bundleDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
    "dependencies",
    "optionalDependencies",
    "transitivePeerDependencies",
    "dev",
    "optional",
];

/// Top-level key priority, matching pnpm's lockfile root-key sort.
const ROOT_KEYS: [&str; 10] = [
    "lockfileVersion",
    "settings",
    "catalogs",
    "overrides",
    "packageExtensionsChecksum",
    "pnpmfileChecksum",
    "untrackedPnpmfileReadPackageHook",
    "patchedDependencies",
    "importers",
    "packages",
];

// pnpm 9 used its root sort order recursively within each importer.
const PNPM9_KEYS: [&str; 13] = [
    "lockfileVersion",
    "settings",
    "catalogs",
    "overrides",
    "packageExtensionsChecksum",
    "pnpmfileChecksum",
    "patchedDependencies",
    "dependencies",
    "optionalDependencies",
    "devDependencies",
    "dependenciesMeta",
    "importers",
    "packages",
];

pub(super) fn importer_order(left: &str, right: &str, pnpm9: bool) -> Ordering {
    priority_cmp(if pnpm9 { &PNPM9_KEYS } else { &ROOT_KEYS }, left, right)
}

pub(super) fn root_order(left: &str, right: &str) -> Ordering {
    priority_cmp(&ROOT_KEYS, left, right)
}

pub(super) fn write_entry(
    output: &mut String,
    section: Option<&str>,
    key: &str,
    value: &Value,
    pnpm9: bool,
) {
    // Render one edited block at a time. Unmodified sections and records retain
    // their source bytes and do not require sorting or serializing a second time.
    let priority: Option<&[&str]> = match (section, key) {
        (Some("importers"), _) if pnpm9 => Some(&PNPM9_KEYS),
        (Some("importers"), _) => Some(&ROOT_KEYS),
        (Some("packages" | "snapshots"), _) => Some(&ORDERED_KEYS),
        (None, "settings" | "catalogs" | "time" | "patchedDependencies") => Some(&[]),
        _ => None,
    };
    let level = usize::from(section.is_some());
    if level == 1 {
        output.push_str("  ");
    }
    if let Some(priority) = priority {
        let mut sorted = value.clone();
        order_fields(&mut sorted, priority);
        render_entry_into(output, key, &sorted, level);
    } else {
        render_entry_into(output, key, value, level);
    }
}

pub(super) fn scalar(value: &str) -> String {
    write_scalar(value, 0, true, true)
}

fn order_fields(value: &mut Value, priority: &[&str]) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = std::mem::take(map).into_iter().collect();
            entries.sort_unstable_by(|(a, _), (b, _)| priority_cmp(priority, a, b));
            for (key, mut value) in entries {
                order_fields(&mut value, priority);
                map.insert(key, value);
            }
        }
        Value::Array(items) => {
            for item in items {
                order_fields(item, priority);
            }
        }
        _ => {}
    }
}

/// Prioritized keys come first in priority order, the rest follow in
/// plain code-unit order.
fn priority_cmp(priority: &[&str], left: &str, right: &str) -> Ordering {
    let rank = |key: &str| priority.iter().position(|entry| *entry == key);
    match (rank(left), rank(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// Render one node. Mirrors the fork's `writeNode`.
///
/// - `level` — current indentation depth.
/// - `block` — whether block style is permitted here (`false` inside flow).
/// - `compact` — whether the first child of a block collection omits its
///   leading newline (the collection sits on the same line as its key).
/// - `object_key` — the map key this value is bound to, driving the
///   single-line and blank-line decisions.
/// - `force_single_line` — propagated `singleLineOnly`: forces single-line
///   scalar styling for keys and for values nested in a single-line map.
fn render(
    value: &Value,
    level: usize,
    block: bool,
    compact: bool,
    object_key: Option<&str>,
    force_single_line: bool,
) -> String {
    match value {
        Value::Object(map) => render_map(map, level, block, compact, object_key),
        Value::Array(seq) => {
            let single_line = object_key.is_some_and(is_single_line_key);
            if block && !seq.is_empty() && !single_line {
                write_block_sequence(seq, level, compact)
            } else {
                write_flow_sequence(seq, level)
            }
        }
        Value::String(string) => write_scalar(string, level, force_single_line, block),
        Value::Bool(boolean) => if *boolean { "true" } else { "false" }.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => "null".to_string(),
    }
}

/// The lockfile's top-level sections and their entries are separated by a
/// blank line; everything nested inside them is not.
fn render_map(
    map: &serde_json::Map<String, Value>,
    level: usize,
    block: bool,
    compact: bool,
    object_key: Option<&str>,
) -> String {
    let single_line = is_single_line_map(object_key, map);
    if !block || map.is_empty() || single_line {
        return write_flow_mapping(map, level, single_line);
    }
    let double_line =
        level == 0 || matches!(object_key, Some("packages" | "importers" | "snapshots"));
    write_block_mapping(map, level, compact, double_line)
}

fn is_single_line_key(key: &str) -> bool {
    SINGLE_LINE_KEYS.contains(&key)
}

/// Whether a map value renders on a single line. `resolution` is single-line
/// except for the nested `variations`/`binary` shapes (detected by the `type`
/// discriminator pnpm's tagged resolutions carry).
fn is_single_line_map(object_key: Option<&str>, map: &serde_json::Map<String, Value>) -> bool {
    match object_key {
        Some(key) if is_single_line_key(key) => true,
        Some("resolution") => !matches!(
            map.get("type").and_then(Value::as_str),
            Some("variations" | "binary")
        ),
        _ => false,
    }
}

/// `generateNextLine`: a newline (doubled when `double_line`) plus this level's
/// indent.
fn next_line(level: usize, double_line: bool) -> String {
    let mut line = String::from("\n");
    if double_line {
        line.push('\n');
    }
    line.extend(std::iter::repeat_n(' ', INDENT * level));
    line
}

/// A rendered key longer than this (measured in UTF-16 code units,
/// matching js-yaml's `state.dump.length > 1024`) is emitted as an
/// explicit `? <key>` / `: <value>` pair: YAML caps a *simple* key at
/// 1024 characters, so an inline key of that length would not re-parse.
const EXPLICIT_KEY_THRESHOLD: usize = 1024;

fn write_block_mapping(
    map: &serde_json::Map<String, Value>,
    level: usize,
    compact: bool,
    double_line: bool,
) -> String {
    let mut result = String::new();
    for (key, value) in map {
        if !compact || !result.is_empty() {
            result.push_str(&next_line(level, double_line));
        }
        render_entry_into(&mut result, key, value, level);
    }
    if result.is_empty() {
        "{}".to_string()
    } else {
        result
    }
}

pub(super) fn render_entry_into(result: &mut String, key: &str, value: &Value, level: usize) {
    let rendered_key = write_scalar(key, level + 1, true, true);
    let explicit_pair = rendered_key.encode_utf16().count() > EXPLICIT_KEY_THRESHOLD;
    if explicit_pair {
        result.push_str("? ");
        result.push_str(&rendered_key);
        result.push_str(&next_line(level, false));
    } else {
        result.push_str(&rendered_key);
    }
    let rendered = render(value, level + 1, true, explicit_pair, Some(key), false);
    result.push(':');
    if !rendered.starts_with('\n') {
        result.push(' ');
    }
    result.push_str(&rendered);
}

fn write_block_sequence(seq: &[Value], level: usize, compact: bool) -> String {
    let mut result = String::new();
    for value in seq {
        let rendered = render(value, level + 1, true, true, None, false);
        if !compact || !result.is_empty() {
            result.push_str(&next_line(level, false));
        }
        result.push('-');
        if !rendered.starts_with('\n') {
            result.push(' ');
        }
        result.push_str(&rendered);
    }
    if result.is_empty() {
        "[]".to_string()
    } else {
        result
    }
}

fn write_flow_mapping(
    map: &serde_json::Map<String, Value>,
    level: usize,
    single_line: bool,
) -> String {
    let mut result = String::new();
    for (key, value) in map {
        if !result.is_empty() {
            result.push_str(", ");
        }
        result.push_str(&write_scalar(key, level, single_line, false));
        result.push_str(": ");
        result.push_str(&render(value, level, false, false, None, single_line));
    }
    format!("{{{result}}}")
}

fn write_flow_sequence(seq: &[Value], level: usize) -> String {
    let mut result = String::new();
    for value in seq {
        if !result.is_empty() {
            result.push_str(", ");
        }
        result.push_str(&render(value, level, false, false, None, false));
    }
    format!("[{result}]")
}

mod implicit;
use implicit::resolves_implicitly;

mod scalars;
use scalars::write_scalar;
