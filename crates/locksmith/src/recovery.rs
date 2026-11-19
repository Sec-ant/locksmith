//! Resolve duplicated snapshot records only when every interpretation agrees.
use std::{collections::BTreeMap, fmt, sync::Arc};

use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::Map;

use crate::{DocumentKind, Lockfile, LockfileError, MergeContext, MergeInput, Value};

const MAX_COMBINATIONS: usize = 16;

/// Merge YAML using final workspace intent, recovering duplicated snapshots.
///
/// Valid inputs use the contextual resolver directly. If a snapshot key occurs
/// repeatedly, all distinct values remain candidates. Every combination must
/// produce the same validated graph, including override precedence. A failed
/// combination or a disagreement returns an error; no candidate is preferred
/// merely because it appears first or last in the text.
///
/// Recovery accepts duplicates only in the top-level `snapshots` mapping and
/// requires string mapping keys and rejects YAML merge keys (`<<`) in the
/// affected input. Other malformed input
/// remains an error. Recovery checks at most 16 combinations and returns
/// `ResolutionRequired` before graph construction if the input exceeds this
/// work budget. Combinations run serially, holding one combination and one
/// serialized result in memory.
/// The library performs no filesystem, network, or package-manager operations.
pub(crate) fn resolve(
    inputs: [MergeInput<'_>; 3],
    context: &MergeContext<'_>,
) -> Result<Lockfile, LockfileError> {
    let mut choices = Vec::new();
    let mut locks = Vec::with_capacity(3);
    for (side, input) in inputs.into_iter().enumerate() {
        let text = match input {
            MergeInput::Parsed(lock) => {
                locks.push(lock.clone());
                continue;
            }
            MergeInput::Yaml(text) => text,
        };
        match Lockfile::parse(text) {
            Ok(lock) => locks.push(lock),
            Err(LockfileError::Parse(_)) => {
                let mut options = crate::source::options(text.len());
                options.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::PassThrough;
                options.merge_keys = serde_saphyr::MergeKeyPolicy::Error;
                let normalized = if text.contains("\r\n") || text.starts_with('\u{feff}') {
                    std::borrow::Cow::Owned(
                        text.trim_start_matches('\u{feff}').replace("\r\n", "\n"),
                    )
                } else {
                    std::borrow::Cow::Borrowed(text)
                };
                let mut documents = BTreeMap::new();
                for range in crate::source::document_ranges(&normalized) {
                    let mut parsed: RecoveredDocument =
                        serde_saphyr::from_str_with_options(&normalized[range], options.clone())?;
                    let kind = crate::source::document_kind(&mut parsed.value, &documents)?;
                    for (key, values) in parsed.alternatives {
                        choices.push(Choice {
                            side,
                            kind,
                            key,
                            values,
                            index: 0,
                        });
                    }
                    documents.insert(kind, Arc::new(Value::Object(parsed.value)));
                }
                if documents.is_empty() {
                    return Err(LockfileError::InvalidDocument("empty lockfile".into()));
                }
                // Invalid source text must never be reused by the serializer.
                locks.push(Lockfile {
                    documents,
                    sources: Vec::new(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    if choices.is_empty() {
        return crate::reconcile::resolve(&locks[0], &locks[1], &locks[2], context);
    }
    let mut combinations = 1usize;
    for choice in &choices {
        if choice.values.len() > MAX_COMBINATIONS / combinations {
            return Err(ambiguity(
                &choices,
                format!(
                    "duplicate snapshot recovery exceeds its {MAX_COMBINATIONS}-combination work budget"
                ),
            ));
        }
        combinations *= choice.values.len();
    }
    let mut reference = BTreeMap::<DocumentKind, String>::new();
    loop {
        let result = crate::reconcile::resolve(&locks[0], &locks[1], &locks[2], context).map_err(
            |error| {
                if context.registry.is_some()
                    && matches!(
                        error,
                        LockfileError::ResolutionRequired {
                            request: Some(_),
                            ..
                        }
                    )
                {
                    return error;
                }
                ambiguity(
                    &choices,
                    format!("a duplicate snapshot interpretation failed: {error}"),
                )
            },
        )?;
        if reference.is_empty() {
            for (&kind, document) in &result.documents {
                let mut text = String::new();
                crate::source::render(kind, document, &result.sources, &mut text);
                crate::verify::verify(&text, document)?;
                reference.insert(kind, text);
            }
        } else if result.documents.len() != reference.len()
            || result.documents.iter().any(|(kind, document)| {
                reference
                    .get(kind)
                    .is_none_or(|text| crate::verify::verify(text, document).is_err())
            })
        {
            return Err(ambiguity(
                &choices,
                "duplicate snapshot interpretations produce different graphs".into(),
            ));
        }
        // The result may share an environment document with an input. Release
        // it before changing the next candidate, without cloning an input tree.
        drop(result);
        if !advance(&mut choices, &mut locks) {
            break;
        }
    }
    // Reparse only the agreed result, after releasing the candidate graphs.
    // Its text is valid and can now be reused byte-for-byte by normal operations.
    drop(locks);
    drop(choices);
    let mut output = String::new();
    for (kind, text) in reference {
        if kind == DocumentKind::Environment || !output.is_empty() {
            output.push_str("---\n");
        }
        output.push_str(&text);
    }
    Lockfile::parse(&output)
}

struct Choice {
    side: usize,
    kind: DocumentKind,
    key: String,
    values: Vec<Value>,
    index: usize,
}

fn ambiguity(choices: &[Choice], message: String) -> LockfileError {
    let choice = choices
        .iter()
        .find(|choice| choice.index != 0)
        .unwrap_or(&choices[0]);
    LockfileError::ResolutionRequired {
        request: None,
        path: vec![
            ["base", "ours", "theirs"][choice.side].into(),
            format!("{:?}", choice.kind),
            "snapshots".into(),
            choice.key.clone(),
        ],
        message,
    }
}

fn advance(choices: &mut [Choice], locks: &mut [Lockfile]) -> bool {
    for choice in choices.iter_mut().rev() {
        choice.index += 1;
        let carry = choice.index == choice.values.len();
        if carry {
            choice.index = 0;
        }
        let document = Arc::get_mut(
            locks[choice.side]
                .documents
                .get_mut(&choice.kind)
                .expect("candidate document"),
        )
        .expect("recovery owns candidate documents");
        document["snapshots"][&choice.key] = choice.values[choice.index].clone();
        if !carry {
            return true;
        }
    }
    false
}

#[derive(Default)]
struct RecoveredDocument {
    value: Map<String, Value>,
    alternatives: BTreeMap<String, Vec<Value>>,
}

impl<'de> Deserialize<'de> for RecoveredDocument {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Root;
        impl<'de> Visitor<'de> for Root {
            type Value = RecoveredDocument;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a lockfile mapping")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut result = RecoveredDocument::default();
                while let Some(TextKey(key)) = map.next_key::<TextKey>()? {
                    if result.value.contains_key(&key) {
                        return Err(de::Error::custom(format!("duplicate mapping key: {key}")));
                    }
                    let value = if key == "snapshots" {
                        let snapshots = map.next_value::<Snapshots>()?;
                        result.alternatives = snapshots.alternatives;
                        Value::Object(snapshots.value)
                    } else {
                        map.next_value::<StrictValue>()?.0
                    };
                    result.value.insert(key, value);
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Root)
    }
}

#[derive(Default)]
struct Snapshots {
    value: Map<String, Value>,
    alternatives: BTreeMap<String, Vec<Value>>,
}
impl<'de> Deserialize<'de> for Snapshots {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries;
        impl<'de> Visitor<'de> for Entries {
            type Value = Snapshots;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a snapshot mapping")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut result = Snapshots::default();
                while let Some(TextKey(key)) = map.next_key::<TextKey>()? {
                    let value = map.next_value::<StrictValue>()?.0;
                    if let Some(first) = result.value.get(&key) {
                        if first != &value {
                            let values = result
                                .alternatives
                                .entry(key)
                                .or_insert_with(|| vec![first.clone()]);
                            if !values.contains(&value) {
                                values.push(value);
                            }
                        }
                    } else {
                        result.value.insert(key, value);
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Entries)
    }
}

/// PassThrough is needed for snapshot alternatives. Every other mapping still
/// enforces unique keys, including maps nested inside a snapshot record.
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Strict;
        impl<'de> Visitor<'de> for Strict {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a value with unique mapping keys")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|value| StrictValue(Value::Number(value)))
                    .ok_or_else(|| de::Error::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut values = Map::new();
                while let Some(TextKey(key)) = map.next_key::<TextKey>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom(format!("duplicate mapping key: {key}")));
                    }
                    values.insert(key, map.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Strict)
    }
}

/// YAML integer-equivalent keys (such as 0xB and 11) must not become two
/// distinct strings when the YAML parser forwards duplicates to our visitor.
struct TextKey(String);
impl<'de> Deserialize<'de> for TextKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Key;
        impl<'de> Visitor<'de> for Key {
            type Value = TextKey;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string mapping key for snapshot recovery")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(TextKey(value.into()))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(TextKey(value))
            }
        }
        deserializer.deserialize_any(Key)
    }
}
