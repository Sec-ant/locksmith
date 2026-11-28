//! Match catalog declarations and resolved pins against dependency references.
use crate::{Value, WorkspaceConfig, model::pin};

pub(crate) fn version_fits(specifier: &str, version: &str, recorded: Option<&str>) -> bool {
    let range = pin::alias(specifier).map_or(specifier, |(_, range)| range);
    match range.parse::<node_semver::Range>() {
        Ok(wanted) => version
            .parse::<node_semver::Version>()
            .is_ok_and(|pin| range == "*" || pin.satisfies(&wanted)),
        Err(_) => recorded == Some(specifier),
    }
}

pub(crate) fn entry_version<'a>(specifier: &str, entry: &'a Value) -> Option<&'a str> {
    let version = entry["version"].as_str()?;
    (entry["specifier"] == specifier && version_fits(specifier, version, Some(specifier)))
        .then_some(version)
}

pub(crate) fn link_entry_fits(specifier: &str, entry: &Value) -> bool {
    entry_version(specifier, entry).is_some()
        || (entry["specifier"] == specifier && entry["version"] == specifier)
}

/// pnpm can retain distinct catalog and importer pins within the declared range.
pub(crate) fn consumer_fits(
    name: &str,
    spec: &str,
    reference: &str,
    entry: Option<&Value>,
    configuration: Option<&WorkspaceConfig>,
) -> bool {
    let effective = pin::pinned_specifier(spec, reference, configuration);
    if let Some(entry) = entry {
        let Some(version) = entry_version(spec, entry) else {
            return false;
        };
        if effective != spec && !version_fits(effective, version, None) {
            return false;
        }
        if pin_matches(name, Some(effective), version, reference) {
            return true;
        }
    }
    pin::range_accepts(name, effective, reference)
}

pub(crate) fn accepts_pin(
    name: &str,
    specifier: &str,
    reference: &str,
    metadata: &Value,
    configuration: Option<&WorkspaceConfig>,
) -> bool {
    if let Some(catalog) = specifier.strip_prefix("catalog:") {
        let catalog = if catalog.is_empty() {
            "default"
        } else {
            catalog
        };
        let Some(entry) = metadata
            .get("catalogs")
            .and_then(|v| v.get(catalog))
            .and_then(|v| v.get(name))
        else {
            return false;
        };
        let Some(version) = entry.get("version").and_then(Value::as_str) else {
            return false;
        };
        let spec = entry["specifier"]
            .as_str()
            .map(|s| pin::pinned_specifier(s, reference, configuration));
        return pin_matches(name, spec, version, reference);
    }
    pin::range_accepts(
        name,
        pin::pinned_specifier(specifier, reference, configuration),
        reference,
    )
}

fn pin_matches(name: &str, spec: Option<&str>, version: &str, reference: &str) -> bool {
    let pin = pin::package_key(reference);
    if let Some(request) = spec.and_then(|s| s.strip_prefix("workspace:"))
        && let Some(path) = version.strip_prefix("file:")
    {
        return pin::file_pin(name, reference).is_some_and(|(package, pinned_path)| {
            pinned_path == path
                && (request.starts_with('.') || package == pin::workspace_request(name, request).0)
        });
    }
    let target = spec.and_then(pin::alias).map_or(name, |(target, _)| target);
    pin::version_from_pin(name, target, pin) == Some(version)
}
