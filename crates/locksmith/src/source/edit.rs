//! Edit only changed mappings in a parsed lock, retaining the original text of siblings.
use std::{collections::BTreeMap, fmt, ops::Range};

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
};
use serde_json::Map;
use serde_saphyr::Spanned;

use super::{FLOW_MAP_CLOSE, FLOW_MAP_OPEN, document_ranges, options, standalone_block, yaml};
use crate::{DocumentKind, Lockfile, LockfileError, Value, schema};

struct Locations(Vec<Spanned<String>>);

impl<'de> Deserialize<'de> for Locations {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Keys;
        impl<'de> Visitor<'de> for Keys {
            type Value = Locations;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a block mapping")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Locations, M::Error> {
                let mut keys = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(key) = map.next_key::<Spanned<String>>()? {
                    keys.push(key);
                    map.next_value::<IgnoredAny>()?;
                }
                Ok(Locations(keys))
            }
        }
        deserializer.deserialize_map(Keys)
    }
}

fn blocks(text: &str, level: usize) -> Option<Vec<(String, Range<usize>)>> {
    let mut opts = options(text.len());
    opts.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::PassThrough;
    let Locations(keys) = serde_saphyr::from_str_with_options(text, opts).ok()?;
    let offsets: Vec<_> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect();
    let mut result = Vec::with_capacity(keys.len());
    let mut end = text.len();
    for key in keys.into_iter().rev() {
        let line = usize::try_from(key.referenced.line()).ok()?;
        let start = *offsets.get(line.checked_sub(1)?)?;
        let raw = text.get(start..end)?;
        if !standalone_block(raw, level * 2) {
            return None;
        }
        result.push((key.value, start..end));
        end = start;
    }
    result.reverse();
    Some(result)
}

fn header_end(text: &str) -> Option<usize> {
    let end = text.find('\n')? + 1;
    let header = text[..end]
        .split_once(" #")
        .map_or(&text[..end], |(key, _)| key);
    header.trim_end().ends_with(':').then_some(end)
}

// Only independent block entries may be removed. The strict parser checks both
// copies separately, including nested keys, then checks the whole edited file.
pub(crate) fn parse_for_repair(
    text: &str,
    reference: Option<&Value>,
) -> Result<Lockfile, LockfileError> {
    let error = match Lockfile::parse(text) {
        Err(error @ LockfileError::Parse(_)) => error,
        result => return result,
    };
    let bom = text.len() - text.trim_start_matches('\u{feff}').len();
    let mut removals = Vec::new();
    let documents = document_ranges(&text[bom..]);
    for doc in &documents {
        let start = bom + doc.start;
        let raw = &text[start..bom + doc.end];
        let Some(fields) = blocks(raw, 0) else {
            continue;
        };
        let kind = reference.and_then(|_| {
            let mut opts = options(raw.len());
            opts.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::PassThrough;
            let mut value = serde_saphyr::from_str_with_options::<Value>(raw, opts).ok()?;
            super::document_kind(value.as_object_mut()?, &BTreeMap::new()).ok()
        });
        for (section, field) in fields {
            duplicate_entries(raw, &section, field, start, kind, reference, &mut removals)?;
        }
    }
    if removals.is_empty() {
        return Err(error);
    }
    removals.sort_unstable_by_key(|range| range.start);
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in removals {
        output.push_str(&text[cursor..range.start]);
        cursor = range.end;
    }
    output.push_str(&text[cursor..]);
    Lockfile::try_from(output)
}

fn duplicate_entries(
    raw: &str,
    section: &str,
    field: Range<usize>,
    document_start: usize,
    kind: Option<DocumentKind>,
    reference: Option<&Value>,
    removals: &mut Vec<Range<usize>>,
) -> Result<(), LockfileError> {
    if !matches!(section, "packages" | "snapshots") {
        return Ok(());
    }
    let field_text = &raw[field.clone()];
    let Some(header) = header_end(field_text) else {
        return Ok(());
    };
    let body = &field_text[header..];
    let Some(mut entries) = blocks(body, 1) else {
        return Ok(());
    };
    let reference =
        reference.filter(|_| section == "snapshots" && kind == Some(DocumentKind::Project));
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    for copies in entries
        .chunk_by(|(a, _), (b, _)| a == b)
        .filter(|c| c.len() > 1)
    {
        let key = &copies[0].0;
        let values = copies
            .iter()
            .map(|(_, range)| {
                let raw = &body[range.clone()];
                serde_saphyr::from_str_with_options::<Value>(raw, options(raw.len()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let keep = if values[1..].iter().all(|value| value == &values[0]) {
            0
        } else {
            reference
                .and_then(|doc| doc["snapshots"].get(key))
                .and_then(|witness| {
                    values
                        .iter()
                        .position(|value| value.get(key) == Some(witness))
                })
                .ok_or_else(|| LockfileError::ResolutionRequired {
                    path: vec![section.into(), key.clone()],
                    message: "duplicate records have different contents".into(),
                    request: None,
                })?
        };
        let offset = document_start + field.start + header;
        removals.extend(copies.iter().enumerate().filter_map(|(i, (_, range))| {
            (i != keep).then_some(offset + range.start..offset + range.end)
        }));
    }
    Ok(())
}

pub(crate) fn project(lock: &Lockfile, after: &Value) -> Result<Lockfile, LockfileError> {
    let source = lock.sources.first().ok_or_else(|| {
        LockfileError::InvalidDocument("missing source text for project edit".into())
    })?;
    let text = &source.text;
    let bom = text.len() - text.trim_start_matches('\u{feff}').len();
    // A project is last when a separate package-manager environment is present.
    let range = document_ranges(&text[bom..]).pop().ok_or_else(|| {
        LockfileError::InvalidDocument(
            "cannot isolate the changed project mapping in the original YAML".into(),
        )
    })?;
    let start = bom + range.start;
    let end = bom + range.end;
    let raw = &text[start..end];
    let mut output = String::with_capacity(text.len());
    output.push_str(&text[..start]);
    let before = lock
        .project()
        .ok_or_else(|| LockfileError::InvalidDocument("missing project document".into()))?;
    let eol = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    if !edit_map(&mut output, raw, before, after, &mut Vec::new(), eol) {
        return Err(LockfileError::InvalidDocument(
            "cannot isolate the changed project mapping in the original YAML".into(),
        ));
    }
    output.push_str(&text[end..]);
    let result = Lockfile::try_from(output)?;
    if result.project() != Some(after)
        || result.document(DocumentKind::Environment) != lock.environment()
        || !schema::same_override_order(after, result.project().expect("verified project"))
    {
        return Err(LockfileError::OutputMismatch);
    }
    Ok(result)
}

fn edit_map<'a>(
    out: &mut String,
    raw: &str,
    before: &Value,
    after: &'a Value,
    path: &mut Vec<&'a str>,
    eol: &str,
) -> bool {
    let (Some(old), Some(new)) = (before.as_object(), after.as_object()) else {
        return false;
    };
    if raw.trim_start().starts_with(FLOW_MAP_OPEN) {
        return edit_flow(out, raw, old, new).is_some();
    }
    let Some(entries) = blocks(raw, path.len()) else {
        return false;
    };
    if entries.is_empty()
        || entries.len() != old.len()
        || entries.iter().any(|(key, _)| !old.contains_key(key))
    {
        return false;
    }
    out.push_str(&raw[..entries[0].1.start]);
    let mut additions: Vec<_> = new.keys().filter(|k| !old.contains_key(*k)).collect();
    let order = |a: &str, b: &str| {
        if path.is_empty() {
            yaml::root_order(a, b)
        } else {
            a.cmp(b)
        }
    };
    additions.sort_by(|a, b| order(a, b));
    let mut additions = additions.into_iter().peekable();
    let top = path.is_empty();
    let spaced = top || (path.len() == 1 && super::SECTIONS.contains(&path[0]));
    for (key, range) in entries {
        while let Some(k) = additions.next_if(|k| {
            if top {
                yaml::root_order(k, &key).is_lt()
            } else {
                k.as_str() < key.as_str()
            }
        }) {
            insert(out, path, k, &new[k], eol, spaced);
        }
        let Some((name, value)) = new.get_key_value(&key) else {
            continue;
        };
        edit_entry(out, &raw[range], name, &old[&key], value, path, eol);
    }
    for k in additions {
        if !out.ends_with(eol) {
            out.push_str(eol);
        }
        if spaced && !out.ends_with(&format!("{eol}{eol}")) {
            out.push_str(eol);
        }
        insert(out, path, k, &new[k], eol, false);
    }
    true
}

fn edit_entry<'a>(
    out: &mut String,
    source: &str,
    name: &'a str,
    before: &Value,
    value: &'a Value,
    path: &mut Vec<&'a str>,
    eol: &str,
) {
    if *before == *value {
        out.push_str(source);
        return;
    }
    if let Some(header) = header_end(source)
        && value.as_object().is_some_and(|map| !map.is_empty())
    {
        let offset = out.len();
        out.push_str(&source[..header]);
        path.push(name);
        let edited = edit_map(out, &source[header..], before, value, path, eol);
        path.pop();
        if edited {
            return;
        }
        out.truncate(offset);
    }
    if let Some((start, _)) = source.match_indices(FLOW_MAP_OPEN).next()
        && let (Some(old), Some(new)) = (before.as_object(), value.as_object())
    {
        let saved = out.len();
        out.push_str(&source[..start]);
        if edit_flow(out, &source[start..], old, new).is_some() {
            return;
        }
        out.truncate(saved);
    }
    if value.is_string()
        && before.is_string()
        && let Ok(map) = serde_saphyr::from_str::<BTreeMap<String, Spanned<String>>>(source)
        && let Some(range) = map.get(name).and_then(span_range)
        && let Some(token) = source.get(range.clone())
        && serde_saphyr::from_str::<String>(token).is_ok_and(|v| *before == v)
    {
        out.push_str(&source[..range.start]);
        out.push_str(&yaml::scalar(value.as_str().expect("string pin")));
        out.push_str(&source[range.end..]);
        return;
    }
    let end = source.trim_end_matches(['\r', '\n']).len();
    write(out, path, name, value, eol);
    out.push_str(&source[end..]);
}

// Flow mappings (including JSON lockfiles) retain each unchanged pair. The
// parser locates quoted keys, so commas and colons in a key or value are data.
fn edit_flow(
    out: &mut String,
    raw: &str,
    old: &Map<String, Value>,
    new: &Map<String, Value>,
) -> Option<()> {
    let Locations(keys) = serde_saphyr::from_str_with_options(raw, options(raw.len())).ok()?;
    let close = raw.rfind(FLOW_MAP_CLOSE)?;
    let mut entries = Vec::new();
    for key in keys {
        let range = span_range(&key)?;
        let rest = raw
            .get(range.end..)?
            .trim_start()
            .strip_prefix(':')?
            .trim_start();
        entries.push((key.value, range.start, raw.len() - rest.len()));
    }
    if entries.len() != old.len() {
        return None;
    }
    let mut result = String::new();
    let first = entries.first().map_or(close, |e| e.1);
    result.push_str(raw.get(..first)?);
    let mut written = false;
    let mut separator = ", ";
    let mut trailing = "";
    for (i, (name, start, value_start)) in entries.iter().enumerate() {
        let end = entries.get(i + 1).map_or(close, |e| e.1);
        let body = raw.get(*value_start..end)?;
        let token = body
            .trim_end()
            .strip_suffix(',')
            .unwrap_or(body.trim_end())
            .trim_end();
        if let Some(next) = new.get(name) {
            if written {
                result.push_str(separator);
            }
            result.push_str(raw.get(*start..*value_start)?);
            if &old[name] == next {
                result.push_str(token);
            } else if let (Some(old), Some(new)) = (old[name].as_object(), next.as_object()) {
                edit_flow(&mut result, token, old, new)?;
            } else {
                result.push_str(&serde_json::to_string(next).ok()?);
            }
            written = true;
        }
        let suffix = &body[token.len()..];
        if i + 1 == entries.len() {
            trailing = suffix;
        } else {
            separator = suffix;
        }
    }
    for (key, value) in new.iter().filter(|(k, _)| !old.contains_key(*k)) {
        if written {
            result.push_str(", ");
        }
        result.push_str(&serde_json::to_string(key).ok()?);
        result.push_str(": ");
        result.push_str(&serde_json::to_string(value).ok()?);
        written = true;
    }
    result.push_str(if written {
        trailing
    } else {
        trailing.trim_start_matches(',')
    });
    result.push_str(raw.get(close..)?);
    out.push_str(&result);
    Some(())
}

fn span_range<T>(value: &Spanned<T>) -> Option<Range<usize>> {
    let span = value.referenced.span();
    let start = usize::try_from(span.byte_offset()?).ok()?;
    let len = usize::try_from(span.byte_len()?).ok()?;
    Some(start..start.checked_add(len)?)
}

fn insert(out: &mut String, path: &[&str], name: &str, value: &Value, eol: &str, spaced: bool) {
    write(out, path, name, value, eol);
    out.push_str(eol);
    if spaced {
        out.push_str(eol);
    }
}

fn write(out: &mut String, path: &[&str], name: &str, value: &Value, eol: &str) {
    let mut entry = String::new();
    if path.len() <= 1 {
        yaml::write_entry(&mut entry, path.first().copied(), name, value);
    } else {
        entry.push_str(&"  ".repeat(path.len()));
        yaml::render_entry_into(&mut entry, name, value, path.len());
    }
    if eol == "\n" {
        out.push_str(&entry);
    } else {
        out.push_str(&entry.replace('\n', eol));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn parse_for_repair_does_not_treat_environment_only_lockfile_as_project() {
        let env_only = "\
lockfileVersion: '9.0'
importers:
  .:
    configDependencies:
      cfg: 1.0.0
snapshots:
  cfg@1.0.0:
    dependencies:
      dep: 1.0.0
  cfg@1.0.0:
    dependencies:
      dep: 2.0.0
";
        let witness = json!({
            "lockfileVersion": "9.0",
            "importers": {".": {}},
            "snapshots": {"cfg@1.0.0": {"dependencies": {"dep": "1.0.0"}}}
        });
        assert!(matches!(
            parse_for_repair(env_only, Some(&witness)),
            Err(LockfileError::ResolutionRequired { .. })
        ));
    }

    #[test]
    fn edit_project_rejects_lockfile_without_source_text() {
        let doc = Arc::new(json!({"lockfileVersion": "9.0", "importers": {".": {}}}));
        let lock = Lockfile::with_documents(BTreeMap::from([(DocumentKind::Project, doc)]), &[]);
        let after = json!({"lockfileVersion": "9.0", "importers": {".": {"extra": true}}});
        assert!(matches!(
            project(&lock, &after),
            Err(LockfileError::InvalidDocument(_))
        ));
    }
}
