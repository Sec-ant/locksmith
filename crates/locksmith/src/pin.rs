//! Interpret dependency specifiers and the package locators recorded in a lockfile.
use crate::WorkspaceConfig;

pub(crate) fn package_key(snapshot: &str) -> &str {
    snapshot.split_once('(').map_or(snapshot, |(key, _)| key)
}

/// A file pin is relative to the lockfile root. Aliases record the package name too.
pub(crate) fn file_pin<'a>(name: &'a str, reference: &'a str) -> Option<(&'a str, &'a str)> {
    let pin = package_key(reference);
    pin.strip_prefix("file:")
        .map(|path| (name, path))
        .or_else(|| pin.split_once("@file:"))
}

pub(crate) fn reference(name: &str, key: &str) -> String {
    key.strip_prefix(name)
        .and_then(|s| s.strip_prefix('@'))
        .unwrap_or(key)
        .into()
}

pub(crate) fn workspace_request<'a>(name: &'a str, request: &'a str) -> (&'a str, &'a str) {
    request
        .rsplit_once('@')
        .filter(|(n, _)| !n.is_empty())
        .unwrap_or((name, request))
}

pub(crate) fn alias(specifier: &str) -> Option<(&str, &str)> {
    let alias = specifier.strip_prefix("npm:")?;
    Some(
        alias
            .rsplit_once('@')
            .filter(|(name, _)| !name.is_empty())
            .unwrap_or((alias, "*")),
    )
}

/// Use the runner's alias rules when known. With no version, a plain pin
/// indicates shorthand and a qualified pin can identify a legacy numeric alias.
pub(crate) fn pinned_specifier<'a>(
    specifier: &'a str,
    pin: &str,
    configuration: Option<&WorkspaceConfig>,
) -> &'a str {
    if let Some(config) = configuration.filter(|config| config.pnpm_version.is_some()) {
        return config.registry_specifier(specifier);
    }
    if let Some(range) = specifier.strip_prefix("npm:")
        && range.parse::<node_semver::Range>().is_ok()
        && package_key(pin).parse::<node_semver::Version>().is_ok()
    {
        range
    } else {
        specifier
    }
}

impl WorkspaceConfig {
    /// pnpm 11.1 added npm:<range> as shorthand for a range on the dependency's own name.
    pub(crate) fn registry_specifier<'a>(&self, specifier: &'a str) -> &'a str {
        if let Some(range) = specifier.strip_prefix("npm:")
            && self
                .pnpm_version
                .as_ref()
                .is_none_or(|v| v >= &node_semver::Version::from((11, 1, 0)))
            && range.parse::<node_semver::Range>().is_ok()
        {
            range
        } else {
            specifier
        }
    }

    pub(crate) fn registry_request<'a>(
        &self,
        name: &'a str,
        specifier: &'a str,
    ) -> (&'a str, &'a str) {
        let specifier = self.registry_specifier(specifier);
        alias(specifier).unwrap_or((name, specifier))
    }
}

pub(crate) fn matches_version(range: &str, version: Option<&str>) -> bool {
    let range = range.parse::<node_semver::Range>();
    let version = version.and_then(|s| s.parse::<node_semver::Version>().ok());
    matches!((range, version), (Ok(range), Some(version)) if version.satisfies(&range))
}

pub(crate) fn selector(value: &str) -> (&str, Option<&str>) {
    value
        .char_indices()
        .skip(1)
        .find(|(_, c)| *c == '@')
        .map_or((value, None), |(i, _)| (&value[..i], Some(&value[i + 1..])))
}

pub(crate) fn range_accepts(name: &str, specifier: &str, reference: &str) -> bool {
    let (target, range) = alias(specifier).unwrap_or((name, specifier));
    matches_version(
        range,
        version_from_pin(name, target, package_key(reference)),
    )
}

pub(crate) fn version_from_pin<'a>(name: &str, target: &str, pin: &'a str) -> Option<&'a str> {
    pin.strip_prefix(target)
        .and_then(|suffix| suffix.strip_prefix('@'))
        .or_else(|| (name == target).then_some(pin))
}
