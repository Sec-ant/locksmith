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

pub(super) fn root_order(left: &str, right: &str) -> Ordering {
    priority_cmp(&ROOT_KEYS, left, right)
}

pub(super) fn write_entry(output: &mut String, section: Option<&str>, key: &str, value: &Value) {
    // Render one edited block at a time. Unmodified sections and records retain
    // their source bytes and do not require sorting or serializing a second time.
    let priority: Option<&[&str]> = match (section, key) {
        (Some("importers"), _) => Some(&ROOT_KEYS),
        (Some("packages" | "snapshots"), _) => Some(&ORDERED_KEYS),
        (None, "settings" | "catalogs" | "time" | "patchedDependencies") => Some(&[]),
        _ => None,
    };
    let level = usize::from(section.is_some());
    if level == 1 {
        output.push_str("  ");
    }
    render_entry_with_priority(output, key, value, level, priority);
}

pub(super) fn scalar(value: &str) -> String {
    write_scalar(value, 0, true, true)
}

fn write_entries<'a>(
    map: &'a serde_json::Map<String, Value>,
    priority: Option<&[&str]>,
    mut visit: impl FnMut(&'a str, &'a Value),
) {
    if let Some(priority) = priority
        && map.len() > 1
        && !(priority.is_empty() && {
            let mut prev: Option<&str> = None;
            map.keys().all(|k| {
                let ordered = prev.is_none_or(|p| p <= k.as_str());
                prev = Some(k.as_str());
                ordered
            })
        })
    {
        let mut entries: Vec<(&str, &Value)> = map.iter().map(|(k, v)| (k.as_str(), v)).collect();
        entries.sort_unstable_by(|(a, _), (b, _)| priority_cmp(priority, a, b));
        for (key, value) in entries {
            visit(key, value);
        }
    } else {
        for (key, value) in map {
            visit(key, value);
        }
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
#[allow(clippy::too_many_arguments)]
fn render_into(
    out: &mut String,
    value: &Value,
    level: usize,
    block: bool,
    compact: bool,
    object_key: Option<&str>,
    force_single_line: bool,
    priority: Option<&[&str]>,
    space_before_inline: bool,
) {
    match value {
        Value::Object(map) => render_map_into(
            out,
            map,
            level,
            block,
            compact,
            object_key,
            priority,
            space_before_inline,
        ),
        Value::Array(seq) => {
            let single_line = object_key.is_some_and(is_single_line_key);
            if block && !seq.is_empty() && !single_line {
                write_block_sequence_into(out, seq, level, compact, priority, space_before_inline);
            } else {
                if space_before_inline {
                    out.push(' ');
                }
                write_flow_sequence_into(out, seq, level, priority);
            }
        }
        Value::String(string) => {
            if space_before_inline {
                out.push(' ');
            }
            write_scalar_into(out, string, level, force_single_line, block);
        }
        Value::Bool(boolean) => {
            if space_before_inline {
                out.push(' ');
            }
            out.push_str(if *boolean { "true" } else { "false" });
        }
        Value::Number(number) => {
            use std::fmt::Write as _;
            if space_before_inline {
                out.push(' ');
            }
            let _ = write!(out, "{number}");
        }
        Value::Null => {
            if space_before_inline {
                out.push(' ');
            }
            out.push_str("null");
        }
    }
}

/// The lockfile's top-level sections and their entries are separated by a
/// blank line; everything nested inside them is not.
#[allow(clippy::too_many_arguments)]
fn render_map_into(
    out: &mut String,
    map: &serde_json::Map<String, Value>,
    level: usize,
    block: bool,
    compact: bool,
    object_key: Option<&str>,
    priority: Option<&[&str]>,
    space_before_inline: bool,
) {
    let single_line = is_single_line_map(object_key, map);
    if !block || map.is_empty() || single_line {
        if space_before_inline {
            out.push(' ');
        }
        write_flow_mapping_into(out, map, level, single_line, priority);
        return;
    }
    let double_line =
        level == 0 || matches!(object_key, Some("packages" | "importers" | "snapshots"));
    write_block_mapping_into(
        out,
        map,
        level,
        compact,
        double_line,
        priority,
        space_before_inline,
    );
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
fn push_next_line(out: &mut String, level: usize, double_line: bool) {
    out.push('\n');
    if double_line {
        out.push('\n');
    }
    out.extend(std::iter::repeat_n(' ', INDENT * level));
}

/// A rendered key longer than this (measured in UTF-16 code units,
/// matching js-yaml's `state.dump.length > 1024`) is emitted as an
/// explicit `? <key>` / `: <value>` pair: YAML caps a *simple* key at
/// 1024 characters, so an inline key of that length would not re-parse.
const EXPLICIT_KEY_THRESHOLD: usize = 1024;

fn write_block_mapping_into(
    out: &mut String,
    map: &serde_json::Map<String, Value>,
    level: usize,
    compact: bool,
    double_line: bool,
    priority: Option<&[&str]>,
    space_before_inline: bool,
) {
    let mut first = true;
    write_entries(map, priority, |key, value| {
        if !compact || !first {
            push_next_line(out, level, double_line);
        } else if space_before_inline {
            out.push(' ');
        }
        first = false;
        render_entry_with_priority(out, key, value, level, priority);
    });
}

pub(super) fn render_entry_into(result: &mut String, key: &str, value: &Value, level: usize) {
    render_entry_with_priority(result, key, value, level, None);
}

fn render_entry_with_priority(
    result: &mut String,
    key: &str,
    value: &Value,
    level: usize,
    priority: Option<&[&str]>,
) {
    let key_start = result.len();
    write_scalar_into(result, key, level + 1, true, true);
    let key_bytes = result.len() - key_start;
    let explicit_pair = key_bytes > EXPLICIT_KEY_THRESHOLD
        && result[key_start..].encode_utf16().count() > EXPLICIT_KEY_THRESHOLD;
    if explicit_pair {
        result.insert_str(key_start, "? ");
        push_next_line(result, level, false);
    }
    result.push(':');
    render_into(
        result,
        value,
        level + 1,
        true,
        explicit_pair,
        Some(key),
        false,
        priority,
        true,
    );
}

fn write_block_sequence_into(
    out: &mut String,
    seq: &[Value],
    level: usize,
    compact: bool,
    priority: Option<&[&str]>,
    space_before_inline: bool,
) {
    for (index, value) in seq.iter().enumerate() {
        if !compact || index > 0 {
            push_next_line(out, level, false);
        } else if space_before_inline {
            out.push(' ');
        }
        out.push('-');
        render_into(
            out,
            value,
            level + 1,
            true,
            true,
            None,
            false,
            priority,
            true,
        );
    }
}

fn write_flow_mapping_into(
    out: &mut String,
    map: &serde_json::Map<String, Value>,
    level: usize,
    single_line: bool,
    priority: Option<&[&str]>,
) {
    out.push('{');
    let mut first = true;
    write_entries(map, priority, |key, value| {
        if !first {
            out.push_str(", ");
        }
        first = false;
        write_scalar_into(out, key, level, single_line, false);
        out.push_str(": ");
        render_into(
            out,
            value,
            level,
            false,
            false,
            None,
            single_line,
            priority,
            false,
        );
    });
    out.push('}');
}

fn write_flow_sequence_into(
    out: &mut String,
    seq: &[Value],
    level: usize,
    priority: Option<&[&str]>,
) {
    out.push('[');
    for (index, value) in seq.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        render_into(
            out, value, level, false, false, None, false, priority, false,
        );
    }
    out.push(']');
}

mod implicit;
use implicit::resolves_implicitly;

mod scalars;
use scalars::{write_scalar, write_scalar_into};
