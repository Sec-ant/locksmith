use std::{collections::BTreeMap, fmt, ops::Range, sync::Arc};

use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::Map;
use serde_saphyr::Spanned;

use crate::{DocumentKind, LockfileError, Value};

pub(crate) const SECTIONS: [&str; 3] = ["importers", "packages", "snapshots"];

#[derive(Debug)]
pub(crate) struct Source {
    pub text: Arc<str>,
    pub documents: BTreeMap<DocumentKind, Arc<Value>>,
    layouts: BTreeMap<DocumentKind, Layout>,
}

#[derive(Debug, Default)]
struct Layout {
    text: Arc<str>,
    prefix: Range<usize>,
    fields: BTreeMap<String, Range<usize>>,
    entries: BTreeMap<String, BTreeMap<String, Range<usize>>>,
}

#[derive(Default)]
struct ParsedMap {
    value: Map<String, Value>,
    lines: Vec<(String, usize)>,
    sections: BTreeMap<String, Vec<(String, usize)>>,
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
            parsed
                .lines
                .push((name.clone(), key.referenced.line() as usize));
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
    pub fn parse(text: &str) -> Result<Self, LockfileError> {
        let text: Arc<str> = Arc::from(text);
        let normalized = if text.contains("\r\n") || text.starts_with('\u{feff}') {
            Arc::from(text.trim_start_matches('\u{feff}').replace("\r\n", "\n"))
        } else {
            Arc::clone(&text)
        };
        let mut parts = Vec::new();
        let mut start = 0;
        let mut offset = 0;
        for line in normalized.split_inclusive('\n') {
            let marker = line.trim_end().strip_prefix("---");
            if marker.is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(" #")) {
                parts.push(start..offset);
                start = offset + line.len();
            }
            offset += line.len();
        }
        parts.push(start..normalized.len());
        let mut documents = BTreeMap::new();
        let mut layouts = BTreeMap::new();
        for range in parts {
            let part = &normalized[range.clone()];
            if part
                .lines()
                .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
            {
                continue;
            }
            // Lockfiles in large workspaces exceed the YAML library's default
            // 250k nodes. Scale raw-input budgets with the file, while retaining
            // its depth and alias-expansion limits.
            let mut parsed: ParsedMap =
                serde_saphyr::from_str_with_options(part, options(part.len()))?;
            let version = parsed
                .value
                .get("lockfileVersion")
                .ok_or_else(|| LockfileError::InvalidDocument("missing lockfileVersion".into()))?;
            if version.as_str() != Some("9.0") && version.as_f64() != Some(9.0) {
                return Err(LockfileError::UnsupportedVersion {
                    version: version.clone(),
                });
            }
            // Numeric 9, numeric 9.0 and quoted '9.0' declare the same schema.
            // Keep their source spelling in Layout, but compare one semantic value.
            parsed
                .value
                .insert("lockfileVersion".into(), Value::String("9.0".into()));
            let root_importer = parsed.value.get("importers").and_then(|v| v.get("."));
            let environment = root_importer.is_some_and(|root| {
                root.get("configDependencies").is_some()
                    || root.get("packageManagerDependencies").is_some()
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

pub(crate) fn options(bytes: usize) -> serde_saphyr::Options {
    let mut budget = serde_saphyr::Budget::default();
    budget.max_nodes = budget.max_nodes.max(bytes);
    budget.max_events = budget.max_events.max(bytes.saturating_mul(2));
    budget.max_total_scalar_bytes = budget.max_total_scalar_bytes.max(bytes);
    serde_saphyr::options! { budget: Some(budget) }
}

/// Key locations come from the YAML parser, including quoted and explicit keys.
/// Only block mappings with standalone keys can be copied as independent text.
fn layout(text: Arc<str>, range: Range<usize>, parsed: &ParsedMap) -> Layout {
    let lines: Vec<&str> = text[range.clone()].split_inclusive('\n').collect();
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    offsets.push(range.start);
    for line in &lines {
        offsets.push(offsets.last().expect("initial offset exists") + line.len());
    }
    let standalone = |positions: &[(String, usize)], indent: usize| {
        positions.windows(2).all(|pair| pair[0].1 < pair[1].1)
            && positions.iter().all(|(_, line)| {
                *line > 0
                    && lines.get(line - 1).is_some_and(|raw| {
                        raw.bytes().take_while(|byte| *byte == b' ').count() == indent
                            && !raw.trim_start().starts_with('{')
                    })
            })
    };
    if parsed.lines.is_empty() || !standalone(&parsed.lines, 0) {
        return Layout::default();
    }
    let mut result = Layout {
        text: Arc::clone(&text),
        prefix: range.start..offsets[parsed.lines[0].1 - 1],
        ..Layout::default()
    };
    for (index, (key, start)) in parsed.lines.iter().enumerate() {
        let end = parsed
            .lines
            .get(index + 1)
            .map_or(lines.len(), |(_, line)| line - 1);
        result
            .fields
            .insert(key.clone(), offsets[start - 1]..offsets[end]);
        if let Some(entries) = parsed.sections.get(key)
            && standalone(entries, 2)
        {
            let mut blocks = BTreeMap::new();
            for (index, (entry, line)) in entries.iter().enumerate() {
                let end = entries.get(index + 1).map_or(end, |(_, next)| next - 1);
                // Flow mappings put several keys on the header line.
                if line > start {
                    blocks.insert(entry.clone(), offsets[line - 1]..offsets[end]);
                }
            }
            result.entries.insert(key.clone(), blocks);
        }
    }
    result
}

pub(crate) fn render(
    kind: DocumentKind,
    document: &Value,
    sources: &[Arc<Source>],
    output: &mut String,
) -> Result<(), LockfileError> {
    if let Some(layout) = sources.iter().find_map(|source| source.layouts.get(&kind)) {
        output.push_str(&layout.text[layout.prefix.clone()]);
    }
    for (key, value) in document
        .as_object()
        .expect("validated document is a mapping")
    {
        let original = sources.iter().find_map(|source| {
            if source.documents.get(&kind)?.get(key)? != value {
                return None;
            }
            let layout = source.layouts.get(&kind)?;
            Some(&layout.text[layout.fields.get(key)?.clone()])
        });
        if let Some(original) = original {
            output.push_str(original);
        } else if SECTIONS.contains(&key.as_str())
            && value.as_object().is_some_and(|map| !map.is_empty())
        {
            output.push_str(&format!("{key}:\n\n"));
            let entries: BTreeMap<_, _> = value
                .as_object()
                .expect("section is a mapping")
                .iter()
                .collect();
            for (entry, value) in entries {
                let original = sources.iter().find_map(|source| {
                    if source.documents.get(&kind)?.get(key)?.get(entry)? != value {
                        return None;
                    }
                    let layout = source.layouts.get(&kind)?;
                    Some(&layout.text[layout.entries.get(key)?.get(entry)?.clone()])
                });
                if let Some(original) = original {
                    output.push_str(original.trim_end_matches('\n'));
                    output.push_str("\n\n");
                } else {
                    let block = singleton(entry, value)?;
                    for line in block.lines() {
                        output.push_str("  ");
                        output.push_str(line);
                        output.push('\n');
                    }
                    output.push('\n');
                }
            }
        } else {
            output.push_str(&singleton(key, value)?);
            output.push('\n');
        }
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    output.truncate(output.trim_end_matches('\n').len());
    output.push('\n');
    Ok(())
}

fn singleton(key: &str, value: &Value) -> Result<String, LockfileError> {
    let object = BTreeMap::from([(key, value)]);
    serde_saphyr::to_string_with_options(
        &object,
        serde_saphyr::ser_options! {
            prefer_block_scalars: false,
            compact_list_indent: false,
        },
    )
    .map_err(LockfileError::Serialize)
}
