//! Build provider scopes and interpret aliased dependency references.
use std::{borrow::Cow, collections::BTreeMap};

use crate::{Value, graph, pin, schema};

use super::identity::root_provider;

pub(crate) fn importer_providers(
    importers: &Value,
    id: &str,
    inherit_root: bool,
) -> BTreeMap<String, String> {
    let mut scope = BTreeMap::new();
    for path in [(inherit_root && id != ".").then_some("."), Some(id)]
        .into_iter()
        .flatten()
    {
        for group in schema::DEPENDENCY_GROUPS {
            for (name, dep) in graph::section(&importers[path], group)
                .into_iter()
                .flatten()
            {
                if let Some(pin) = dep["version"].as_str() {
                    scope.insert(name.clone(), root_provider(path, pin));
                }
            }
        }
    }
    scope
}

pub(crate) fn canonical_providers(
    providers: &BTreeMap<String, String>,
) -> Cow<'_, BTreeMap<String, String>> {
    let mut result = Cow::Borrowed(providers);
    for value in providers.values() {
        if value.starts_with("link:") {
            continue;
        }
        if let Some((name, _)) = pin::package_key(value).rsplit_once('@') {
            if providers.contains_key(name) {
                continue;
            }
            let version = &value[name.len() + 1..];
            let replace = result.get(name).is_none_or(|old| {
                matches!((pin::package_key(old).parse::<node_semver::Version>(), pin::package_key(version).parse::<node_semver::Version>()),
                    (Ok(old), Ok(new)) if new > old)
            });
            if replace {
                result.to_mut().insert(name.into(), version.into());
            }
        }
    }
    result
}

pub(crate) fn bound_reference<'a>(
    name: &str,
    actual: &str,
    lookup: impl Fn(&str) -> Option<&'a str>,
) -> Option<String> {
    if let Some(value) = lookup(name) {
        return Some(value.into());
    }
    let (target, _) = pin::package_key(actual).rsplit_once('@')?;
    let value = lookup(target)?;
    Some(
        if value.starts_with("link:") || pin::package_key(value).contains('@') {
            value.into()
        } else {
            format!("{target}@{value}")
        },
    )
}

pub(crate) fn canonical_binding<'a>(name: &'a str, reference: &'a str) -> (&'a str, &'a str) {
    if reference.starts_with("link:") {
        return (name, reference);
    }
    if let Some((actual, _)) = pin::package_key(reference).rsplit_once('@') {
        (actual, &reference[actual.len() + 1..])
    } else {
        (name, reference)
    }
}

#[cfg(test)]
mod tests {
    use super::bound_reference;

    #[test]
    fn aliased_peer_fallback_preserves_the_selected_package_reference() {
        for (binding, expected) in [
            ("2.0.0", "@org/real@2.0.0"),
            ("replacement@2.0.0", "replacement@2.0.0"),
            ("link:packages/real", "link:packages/real"),
        ] {
            let reference = bound_reference("compat", "@org/real@1.0.0", |name| {
                (name == "@org/real").then_some(binding)
            });
            assert_eq!(reference.as_deref(), Some(expected));
        }
    }
}
