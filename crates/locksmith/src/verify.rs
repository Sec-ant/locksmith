//! Compare serialized YAML to an existing value while deserializing. No second
//! document tree or source-layout index is allocated during output verification.
use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
    fmt,
};

use serde::{
    Deserialize,
    de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
};

use crate::{LockfileError, Value};

pub(crate) fn verify(text: &str, expected: &Value) -> Result<(), LockfileError> {
    #[cfg(feature = "bench-profile")]
    let _span = crate::profile::span("verify_yaml");
    serde_saphyr::with_deserializer_from_str_with_options(
        text,
        crate::source::options(text.len()),
        |de| {
            Expected {
                value: expected,
                root: true,
                version: false,
                ordered: false,
            }
            .deserialize(de)
        },
    )
    .map_err(|_| LockfileError::OutputMismatch)
}

#[derive(Deserialize)]
struct Key<'a>(#[serde(borrow)] Cow<'a, str>);

struct Expected<'a> {
    value: &'a Value,
    root: bool,
    version: bool,
    ordered: bool,
}
impl Expected<'_> {
    fn check<E: de::Error>(&self, equal: bool) -> Result<(), E> {
        if equal {
            Ok(())
        } else {
            Err(E::custom("serialized lockfile differs from its data"))
        }
    }
}
impl<'de> DeserializeSeed<'de> for Expected<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Expected<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("the original lockfile value")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.check(self.value.is_null())
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<(), E> {
        self.check(self.value.as_bool() == Some(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<(), E> {
        self.check(self.value.as_str() == Some(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<(), E> {
        self.check((self.version && v == 9) || self.value == &Value::from(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<(), E> {
        self.check((self.version && v == 9) || self.value == &Value::from(v))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<(), E> {
        self.check((self.version && v == 9.0) || self.value == &Value::from(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let expected = self
            .value
            .as_array()
            .ok_or_else(|| de::Error::custom("expected a different value"))?;
        for value in expected {
            if seq
                .next_element_seed(Expected {
                    value,
                    root: false,
                    version: false,
                    ordered: false,
                })?
                .is_none()
            {
                return Err(de::Error::custom("missing sequence element"));
            }
        }
        self.check(seq.next_element::<IgnoredAny>()?.is_none())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let expected = self
            .value
            .as_object()
            .ok_or_else(|| de::Error::custom("expected a different value"))?;
        let mut count = 0;
        let mut override_order: BTreeMap<_, BTreeMap<_, VecDeque<_>>> = BTreeMap::new();
        if self.ordered {
            for key in expected.keys() {
                let (name, parent) = crate::schema::override_scope(key);
                override_order
                    .entry(name)
                    .or_default()
                    .entry(parent.unwrap_or(""))
                    .or_default()
                    .push_back(key.as_str());
            }
        }
        while let Some(Key(key)) = map.next_key()? {
            let key = key.as_ref();
            if self.ordered {
                let (name, parent) = crate::schema::override_scope(key);
                self.check(
                    override_order
                        .get_mut(name)
                        .and_then(|parents| parents.get_mut(parent.unwrap_or("")))
                        .and_then(VecDeque::pop_front)
                        == Some(key),
                )?;
            }
            let value = expected
                .get(key)
                .ok_or_else(|| de::Error::custom("unexpected mapping key"))?;
            map.next_value_seed(Expected {
                value,
                root: false,
                version: self.root && key == "lockfileVersion",
                ordered: self.root && key == "overrides",
            })?;
            count += 1;
        }
        self.check(count == expected.len())
    }
}
