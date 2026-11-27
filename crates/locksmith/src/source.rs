pub(crate) mod edit;
mod yaml;

use std::{collections::BTreeMap, fmt, ops::Range, sync::Arc};

use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::Map;
use serde_saphyr::Spanned;

use crate::{DocumentKind, LockfileError, Value, schema};

pub(crate) const SECTIONS: [&str; 3] = ["importers", "packages", "snapshots"];

#[derive(Debug)]
pub(crate) struct Source {
    pub text: Arc<String>,
    pub documents: BTreeMap<DocumentKind, Arc<Value>>,
    layouts: BTreeMap<DocumentKind, Layout>,
}

#[derive(Debug, Default)]
struct Layout {
    text: Arc<String>,
    range: Range<usize>,
    prefix: Range<usize>,
    fields: BTreeMap<String, Range<usize>>,
    entries: BTreeMap<String, Vec<(EntryKey, Range<usize>)>>,
    rewritten_sections: Vec<String>,
    original_spelling: BTreeMap<String, BTreeMap<String, Range<usize>>>,
}

#[derive(Debug)]
enum EntryKey {
    Source(Range<usize>),
    Decoded(Box<str>),
}

impl EntryKey {
    fn from_source(key: &str, text: &str, header: Range<usize>) -> Self {
        let spelling = &text[header.clone()];
        for quote in ["", "'", "\""] {
            if spelling
                .strip_prefix(quote)
                .and_then(|s| s.strip_prefix(key))
                .and_then(|s| s.strip_prefix(quote))
                .is_some_and(|s| s.starts_with(':'))
            {
                let start = header.start + quote.len();
                return Self::Source(start..start + key.len());
            }
        }
        Self::Decoded(key.into())
    }
    fn text<'a>(&'a self, source: &'a str) -> &'a str {
        match self {
            Self::Source(range) => &source[range.clone()],
            Self::Decoded(key) => key,
        }
    }
}

#[derive(Default)]
struct ParsedMap {
    value: Map<String, Value>,
    lines: Vec<usize>,
    sections: BTreeMap<String, Vec<usize>>,
}

struct MapVisitor {
    root: bool,
}

impl<'de> Visitor<'de> for MapVisitor {
    type Value = ParsedMap;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a lockfile mapping")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<ParsedMap, M::Error> {
        let mut parsed = ParsedMap::default();
        while let Some(key) = map.next_key::<Spanned<String>>()? {
            let name = key.value;
            parsed.lines.push(key.referenced.line() as usize);
            let value = if self.root && SECTIONS.contains(&name.as_str()) {
                let entries = map.next_value::<Entries>()?.0;
                parsed.sections.insert(name.clone(), entries.lines);
                Value::Object(entries.value)
            } else {
                map.next_value::<Value>()?
            };
            parsed.value.insert(name, value);
        }
        Ok(parsed)
    }
}

impl<'de> Deserialize<'de> for ParsedMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(MapVisitor { root: true })
    }
}

struct Entries(ParsedMap);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_map(MapVisitor { root: false })
            .map(Self)
    }
}

impl Source {
    pub fn parse(text: String) -> Result<Self, LockfileError> {
        let text = Arc::new(text);
        let normalized = if text.contains("\r\n") || text.starts_with('\u{feff}') {
            Arc::new(text.trim_start_matches('\u{feff}').replace("\r\n", "\n"))
        } else {
            Arc::clone(&text)
        };
        let mut documents = BTreeMap::new();
        let mut layouts = BTreeMap::new();
        for range in document_ranges(&normalized) {
            let part = &normalized[range.clone()];
            // Lockfiles in large workspaces exceed the YAML library's default
            // 250k nodes. Scale raw-input budgets with the file, while retaining
            // its depth and alias-expansion limits.
            let mut parsed: ParsedMap =
                serde_saphyr::from_str_with_options(part, options(part.len()))?;
            let kind = document_kind(&mut parsed.value, &documents)?;
            if let Some(packages) = parsed.value.get("packages").and_then(Value::as_object) {
                crate::schema::check_peer_declarations(kind, packages)?;
            }
            layouts.insert(kind, layout(Arc::clone(&normalized), range, &parsed));
            documents.insert(kind, Arc::new(Value::Object(parsed.value)));
        }
        if documents.is_empty() {
            return Err(LockfileError::InvalidDocument("empty lockfile".into()));
        }
        Ok(Self {
            text,
            documents,
            layouts,
        })
    }
}

pub(crate) fn document_ranges(text: &str) -> Vec<Range<usize>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let markers = text
        .starts_with("---")
        .then_some(0)
        .into_iter()
        .chain(text.match_indices("\n---").map(|(offset, _)| offset + 1));
    for offset in markers {
        let end = text[offset..]
            .find('\n')
            .map_or(text.len(), |newline| offset + newline + 1);
        let suffix = text[offset + 3..end].trim_end_matches(['\r', '\n']);
        let indicator = suffix.trim_start_matches([' ', '\t']);
        if indicator.is_empty() || (suffix.starts_with([' ', '\t']) && indicator.starts_with('#')) {
            parts.push(start..offset);
            start = end;
        }
    }
    parts.push(start..text.len());
    parts.retain(|range| {
        !text[range.clone()]
            .lines()
            .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
    });
    parts
}

pub(crate) fn document_kind(
    value: &mut Map<String, Value>,
    documents: &BTreeMap<DocumentKind, Arc<Value>>,
) -> Result<DocumentKind, LockfileError> {
    let version = value
        .get("lockfileVersion")
        .ok_or_else(|| LockfileError::InvalidDocument("missing lockfileVersion".into()))?;
    if version.as_str() != Some("9.0") && version.as_f64() != Some(9.0) {
        return Err(LockfileError::UnsupportedVersion {
            version: version.clone(),
        });
    }
    // Numeric 9, numeric 9.0 and quoted '9.0' declare the same schema.
    // Keep their source spelling in Layout, but compare one semantic value.
    value.insert("lockfileVersion".into(), Value::String("9.0".into()));
    let root_importer = value.get("importers").and_then(|v| v.get("."));
    let environment = root_importer.is_some_and(|root| {
        root.get("configDependencies").is_some() || root.get("packageManagerDependencies").is_some()
    });
    let kind = if environment {
        DocumentKind::Environment
    } else {
        DocumentKind::Project
    };
    if documents.contains_key(&kind) {
        return Err(LockfileError::InvalidDocument(format!(
            "multiple {kind:?} documents"
        )));
    }
    if kind == DocumentKind::Environment && documents.contains_key(&DocumentKind::Project) {
        return Err(LockfileError::InvalidDocument(
            "environment document must precede project document".into(),
        ));
    }
    Ok(kind)
}

pub(crate) fn options(bytes: usize) -> serde_saphyr::Options {
    let mut budget = serde_saphyr::Budget::default();
    budget.max_nodes = budget.max_nodes.max(bytes);
    budget.max_events = budget.max_events.max(bytes.saturating_mul(2));
    budget.max_total_scalar_bytes = budget.max_total_scalar_bytes.max(bytes);
    serde_saphyr::options! { budget: Some(budget) }
}

/// Key locations come from the YAML parser, including quoted and explicit keys.
/// Only block mappings with standalone keys can be copied as independent text.
fn layout(text: Arc<String>, range: Range<usize>, parsed: &ParsedMap) -> Layout {
    let mut offsets = Vec::new();
    offsets.push(range.start);
    for line in text[range.clone()].split_inclusive('\n') {
        offsets.push(offsets.last().expect("initial offset exists") + line.len());
    }
    let standalone = |positions: &[usize], indent: usize| {
        positions.windows(2).all(|pair| pair[0] < pair[1])
            && positions.iter().all(|line| {
                *line > 0
                    && offsets.get(*line).is_some_and(|end| {
                        let raw = &text[offsets[line - 1]..*end];
                        raw.bytes().take_while(|byte| *byte == b' ').count() == indent
                            && !raw.trim_start().starts_with('{')
                    })
            })
    };
    if parsed.lines.is_empty() || !standalone(&parsed.lines, 0) {
        return Layout {
            text,
            range,
            ..Layout::default()
        };
    }
    let mut result = Layout {
        text: Arc::clone(&text),
        prefix: range.start..offsets[parsed.lines[0] - 1],
        range: range.clone(),
        ..Layout::default()
    };
    for (index, (key, start)) in parsed.value.keys().zip(&parsed.lines).enumerate() {
        let end = parsed
            .lines
            .get(index + 1)
            .map_or(offsets.len() - 1, |line| line - 1);
        result
            .fields
            .insert(key.clone(), offsets[start - 1]..offsets[end]);
        if let Some(entries) = parsed.sections.get(key)
            && standalone(entries, 2)
        {
            let mut blocks = Vec::with_capacity(entries.len());
            let keys = parsed.value[key]
                .as_object()
                .expect("parsed section")
                .keys();
            for (index, (entry, &line)) in keys.zip(entries).enumerate() {
                let end = entries.get(index + 1).map_or(end, |next| next - 1);
                if line > *start {
                    let block = offsets[line - 1]..offsets[end];
                    // Other YAML writers can expand pnpm's inline resolution or use
                    // double quotes for scoped package IDs. Re-render these records
                    // when composing a lock so a subsequent pnpm write is stable.
                    let raw = &text[block.clone()];
                    if matches!(key.as_str(), "packages" | "snapshots")
                        && (raw.starts_with("  \"@")
                            || raw.contains("\n      \"@")
                            || (key == "packages"
                                && raw.contains("\n    resolution:\n      integrity:")))
                    {
                        result
                            .original_spelling
                            .entry(key.clone())
                            .or_default()
                            .insert(entry.clone(), block);
                        continue;
                    }
                    let name = EntryKey::from_source(entry, &text, block.start + 2..offsets[line]);
                    blocks.push((name, block));
                }
            }
            if !blocks.iter().map(|(key, _)| key.text(&text)).is_sorted() {
                blocks.sort_unstable_by(|a, b| a.0.text(&text).cmp(b.0.text(&text)));
            }
            if blocks.len() < entries.len() {
                result.rewritten_sections.push(key.clone());
            }
            result.entries.insert(key.clone(), blocks);
        }
    }
    result
}

fn ordered_importer(record: &Value) -> bool {
    schema::DEPENDENCY_GROUPS.into_iter().all(|group| {
        record
            .get(group)
            .and_then(Value::as_object)
            .is_none_or(|deps| {
                deps.keys()
                    .is_sorted_by(|a, b| yaml::root_order(a, b).is_le())
            })
    })
}

/// Use the installed order when it preserves the relative order of projects
/// from the baseline, including when a project was added or removed.
pub(crate) fn importer_order<'a>(
    sources: &'a [Arc<Source>],
    kind: DocumentKind,
    doc: &Value,
) -> Option<&'a Map<String, Value>> {
    let output = doc["importers"].as_object()?;
    let maps = || {
        sources
            .iter()
            .filter_map(|s| s.documents.get(&kind)?["importers"].as_object())
    };
    maps().rev().find(|order| {
        order.len() == output.len()
            && order.keys().all(|k| output.contains_key(k))
            && maps().all(|source| {
                source
                    .keys()
                    .filter(|k| order.contains_key(*k))
                    .eq(order.keys().filter(|k| source.contains_key(*k)))
            })
    })
}

impl Source {
    pub(crate) fn reusable_field(
        &self,
        kind: DocumentKind,
        key: &str,
        preserve_spelling: bool,
        keep_importer_order: bool,
    ) -> bool {
        if !SECTIONS.contains(&key) && key != "catalogs" {
            return true;
        }
        if !preserve_spelling
            && self.layouts[&kind]
                .rewritten_sections
                .iter()
                .any(|section| section == key)
        {
            return false;
        }
        self.documents[&kind][key]
            .as_object()
            .is_none_or(|entries| {
                (key == "importers" && keep_importer_order || entries.keys().is_sorted())
                    && match key {
                        "importers" => entries.values().all(ordered_importer),
                        "catalogs" => entries
                            .values()
                            .all(|v| v.as_object().is_none_or(|m| m.keys().is_sorted())),
                        _ => true,
                    }
            })
    }
}

pub(crate) fn render(
    kind: DocumentKind,
    document: &Value,
    sources: &[Arc<Source>],
    preserve_baseline: bool,
    output: &mut String,
) {
    #[cfg(feature = "bench-profile")]
    let _span = crate::profile::span("render_source_yaml");
    if kind == DocumentKind::Environment
        && let Some(layout) = sources.iter().find_map(|source| {
            (source.documents.get(&kind)?.as_ref() == document)
                .then(|| source.layouts.get(&kind))
                .flatten()
        })
    {
        output.push_str(&layout.text[layout.range.clone()]);
        if !output.ends_with('\n') {
            output.push('\n');
        }
        return;
    }
    if let Some(layout) = sources.iter().find_map(|source| source.layouts.get(&kind)) {
        output.push_str(&layout.text[layout.prefix.clone()]);
    }
    let mut fields: Vec<_> = document
        .as_object()
        .expect("validated document is a mapping")
        .iter()
        .collect();
    fields.sort_unstable_by(|(a, _), (b, _)| yaml::root_order(a, b));
    let order = preserve_baseline
        .then(|| importer_order(sources, kind, document))
        .flatten();
    for (key, value) in fields {
        let original = sources.iter().enumerate().find_map(|(i, source)| {
            let original = source.documents.get(&kind)?;
            if original.get(key)? != value
                || !source.reusable_field(kind, key, preserve_baseline && i == 0, order.is_some())
                || (key == "overrides" && !schema::same_override_order(original, document))
            {
                return None;
            }
            let layout = source.layouts.get(&kind)?;
            Some(&layout.text[layout.fields.get(key)?.clone()])
        });
        if let Some(original) = original {
            output.push_str(original.trim_end_matches('\n'));
            output.push_str("\n\n");
        } else if SECTIONS.contains(&key.as_str())
            && value.as_object().is_some_and(|map| !map.is_empty())
        {
            output.push_str(&format!("{key}:\n\n"));
            let map = value.as_object().expect("section is a mapping");
            let entries = if let Some(order) = order.filter(|_| key == "importers") {
                order.keys().map(|k| (k, &map[k])).collect()
            } else {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_unstable_by_key(|(key, _)| *key);
                entries
            };
            for (entry, value) in entries {
                let original = sources.iter().enumerate().find_map(|(i, source)| {
                    let record = source.documents.get(&kind)?.get(key)?.get(entry)?;
                    if record != value || (key == "importers" && !ordered_importer(record)) {
                        return None;
                    }
                    let layout = source.layouts.get(&kind)?;
                    if preserve_baseline
                        && i == 0
                        && let Some(range) =
                            layout.original_spelling.get(key).and_then(|m| m.get(entry))
                    {
                        return Some(&layout.text[range.clone()]);
                    }
                    let entries = layout.entries.get(key)?;
                    let index = entries
                        .binary_search_by(|(name, _)| name.text(&layout.text).cmp(entry))
                        .ok()?;
                    Some(&layout.text[entries[index].1.clone()])
                });
                if let Some(original) = original {
                    output.push_str(original.trim_end_matches('\n'));
                    output.push_str("\n\n");
                } else {
                    yaml::write_entry(output, Some(key), entry, value);
                    output.push_str("\n\n");
                }
            }
        } else {
            yaml::write_entry(output, None, key, value);
            output.push_str("\n\n");
        }
    }
    output.truncate(output.trim_end_matches('\n').len());
    output.push('\n');
}
