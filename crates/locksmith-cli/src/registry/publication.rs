use serde::{
    Deserialize, Deserializer,
    de::{Error, IgnoredAny, MapAccess, Visitor},
};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, fmt};

pub(crate) const RESOLUTION_FIELDS: &[&str] = &[
    "name",
    "version",
    "dist",
    "_npmUser",
    "deprecated",
    "dependencies",
    "optionalDependencies",
    "dependenciesMeta",
    "peerDependencies",
    "peerDependenciesMeta",
    "bundledDependencies",
    "bundleDependencies",
    "os",
    "cpu",
    "libc",
    "engines",
    "bin",
    "directories",
];

#[derive(serde::Serialize)]
#[serde(transparent)]
pub(super) struct Publication(Box<RawValue>);

impl Publication {
    pub(super) fn into_raw(self) -> Box<RawValue> {
        self.0
    }
}

impl<'de> Deserialize<'de> for Publication {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Publication;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a published package manifest")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    // Retain the fields used by the resolver and supply-chain
                    // checks. Registry READMEs can be many MB or end mid-UTF-8.
                    if RESOLUTION_FIELDS.contains(&key.as_str()) {
                        fields.insert(key, map.next_value::<Box<RawValue>>()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                serde_json::value::to_raw_value(&fields)
                    .map(Publication)
                    .map_err(M::Error::custom)
            }
        }
        deserializer.deserialize_map(Fields)
    }
}
