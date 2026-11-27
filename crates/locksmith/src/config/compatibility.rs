//! Determine whether settings changes allow locked package records to be reused.
use std::collections::BTreeMap;

use crate::{LockfileError, Value, pin};

use super::{INCLUDE_RESOLUTION_SETTINGS, ResolutionSettings, WorkspaceConfig};

impl WorkspaceConfig {
    pub(crate) fn matches_optional_header_for_repair(
        &self,
        document: &Value,
    ) -> Result<bool, LockfileError> {
        let recorded = optional_patterns(document.get("ignoredOptionalDependencies"), true)?;
        let rules = self.resolution.ignored_optional_dependencies.as_ref();
        let mut configured = optional_patterns(rules, true)?;
        if recorded == configured {
            return Ok(true);
        }
        // pnpm 9–11 also write sorted headers for mixed positive and negative rules.
        if self
            .pnpm_version
            .as_ref()
            .is_some_and(|v| (9..=11).contains(&v.major()))
        {
            configured.sort_unstable();
            return Ok(recorded == configured);
        }
        Ok(false)
    }
}

/// Order matters only for mixed positive and negative patterns. Serialized
/// pnpm headers can be sorted even when the configured matcher uses authored order.
pub(crate) fn optional_patterns(
    value: Option<&Value>,
    preserve_mixed_order: bool,
) -> Result<Vec<&str>, LockfileError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let invalid = || {
        LockfileError::InvalidDocument(
            "ignoredOptionalDependencies must be an array of strings".into(),
        )
    };
    let mut names: Vec<_> = value
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|v| v.as_str().ok_or_else(invalid))
        .collect::<Result<_, _>>()?;
    if !preserve_mixed_order
        || names.iter().all(|n| n.starts_with('!'))
        || names.iter().all(|n| !n.starts_with('!'))
    {
        names.sort_unstable();
    }
    Ok(names)
}

impl ResolutionSettings {
    pub(crate) fn ignored_optional_dependencies_match(
        &self,
        document: &Value,
    ) -> Result<bool, LockfileError> {
        Ok(
            optional_patterns(document.get("ignoredOptionalDependencies"), false)?
                == optional_patterns(self.ignored_optional_dependencies.as_ref(), false)?,
        )
    }
}

// pnpm treats only `*` as a wildcard. The last matching rule wins;
// a list of only negative rules includes every name they do not match.
pub(crate) fn matches_patterns<'a>(
    patterns: impl DoubleEndedIterator<Item = &'a str>,
    name: &str,
) -> bool {
    let mut patterns = patterns.rev().peekable();
    let mut only_negative = patterns.peek().is_some();
    for pattern in patterns {
        let negative = pattern.strip_prefix('!');
        if wildcard(negative.unwrap_or(pattern), name) {
            return negative.is_none();
        }
        only_negative &= negative.is_some();
    }
    only_negative
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().expect("one split part");
    let Some(mut rest) = value.strip_prefix(first) else {
        return false;
    };
    let mut parts = parts.peekable();
    if parts.peek().is_none() {
        return rest.is_empty();
    }
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        let Some(offset) = rest.find(part) else {
            return false;
        };
        rest = &rest[offset + part.len()..];
    }
    true
}

const EXTENSION_GROUPS: [&str; 4] = [
    "dependencies",
    "optionalDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
];
pub(crate) type ExtensionFields<'a> = [BTreeMap<&'a str, &'a Value>; 4];
type EffectiveExtensions<'a> = [ExtensionFields<'a>; 2];
type ExtensionRules<'a> = Vec<(&'a str, &'a Value)>;

pub(crate) fn extension_fields<'a>(
    rules: &[(&'a str, &'a Value)],
    name: &str,
    version: &str,
) -> Option<ExtensionFields<'a>> {
    let mut fields: ExtensionFields<'a> = Default::default();
    for (selector, extension) in rules {
        if crate::schema::parent_matches(selector, name, version) {
            for (i, group) in EXTENSION_GROUPS.iter().enumerate() {
                if let Some(values) = extension.get(*group) {
                    for (dep, value) in values.as_object()? {
                        fields[i].entry(dep).or_insert(value);
                    }
                }
            }
        }
    }
    Some(fields)
}

pub(crate) struct ExtensionAddition<'a> {
    pub group: &'static str,
    pub name: &'a str,
    pub specifier: &'a str,
}

/// Compare global settings once and index changed extensions by package name.
/// An extension for one package does not invalidate unrelated locked packages.
pub(crate) struct ResolutionCompatibility<'a> {
    global: bool,
    extensions: BTreeMap<&'a str, [ExtensionRules<'a>; 2]>,
    additions: BTreeMap<&'a str, Vec<ExtensionAddition<'a>>>,
}

impl<'a> ResolutionCompatibility<'a> {
    pub(crate) fn new(
        source: &'a ResolutionSettings,
        target: &'a ResolutionSettings,
        document: &'a Value,
    ) -> Self {
        let affects_resolution = |(key, value): &(&String, &Value)| {
            // The flag controls recording and frozen checks; it changes no pins.
            key.as_str() != INCLUDE_RESOLUTION_SETTINGS
                && (key.as_str() != "linkWorkspacePackages" || *value == "deep")
        };
        let global = source.lock == target.lock
            && source.dedupe_peer_dependents == target.dedupe_peer_dependents
            && source.optional_peer_scope == target.optional_peer_scope
            && source.workspace_root_peers == target.workspace_root_peers
            && source.inject_workspace_packages == target.inject_workspace_packages
            && source.ignored_optional_dependencies == target.ignored_optional_dependencies
            && source.pnpmfile_checksum == target.pnpmfile_checksum
            && source
                .parameters
                .iter()
                .filter(affects_resolution)
                .eq(target.parameters.iter().filter(affects_resolution));
        let mut result = Self {
            global,
            extensions: BTreeMap::new(),
            additions: BTreeMap::new(),
        };
        for (side, extensions) in [&source.package_extensions, &target.package_extensions]
            .into_iter()
            .enumerate()
        {
            let Some(extensions) = extensions else {
                continue;
            };
            let Some(extensions) = extensions.as_object() else {
                result.global = false;
                continue;
            };
            for (selector, extension) in extensions {
                let (_, name, _) = crate::schema::override_selector(selector);
                result.extensions.entry(name).or_default()[side].push((selector, extension));
            }
        }
        result.extensions.retain(|_, rules| rules[0] != rules[1]);
        if result.global && !result.extensions.is_empty() {
            for (key, _) in crate::schema::section(document, "packages")
                .into_iter()
                .flatten()
            {
                if !result.unchanged(key)
                    && let Some(additions) = result.additional_dependencies(key)
                    && (additions.is_empty()
                        || (source.pnpmfile_checksum.is_none()
                            && document.get("pnpmfileChecksum").is_none()))
                {
                    result.additions.insert(key, additions);
                }
            }
        }
        result
    }

    pub(crate) fn accepts(&self, key: &str) -> bool {
        self.unchanged(key) || self.additions.contains_key(pin::package_key(key))
    }

    pub(crate) fn additions(&self, key: &str) -> &[ExtensionAddition<'a>] {
        self.additions
            .get(pin::package_key(key))
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn effective_extensions(&self, key: &str) -> Option<EffectiveExtensions<'a>> {
        if !self.global {
            return None;
        }
        let (name, version) = pin::package_key(key).rsplit_once('@')?;
        let rules = self.extensions.get(name)?;
        Some([
            extension_fields(&rules[0], name, version)?,
            extension_fields(&rules[1], name, version)?,
        ])
    }

    fn additional_dependencies(&self, key: &str) -> Option<Vec<ExtensionAddition<'a>>> {
        let effective = self.effective_extensions(key)?;
        if effective[0][2..] != effective[1][2..] {
            return None;
        }
        let mut additions = Vec::new();
        for (field, group) in EXTENSION_GROUPS[..2].iter().enumerate() {
            let before = &effective[0][field];
            let after = &effective[1][field];
            if before
                .iter()
                .any(|(name, value)| after.get(name) != Some(value))
            {
                return None;
            }
            for (name, value) in after {
                if !before.contains_key(name) {
                    additions.push(ExtensionAddition {
                        group,
                        name,
                        specifier: value.as_str()?,
                    });
                }
            }
        }
        Some(additions)
    }

    pub(crate) fn unchanged(&self, key: &str) -> bool {
        if !self.global {
            return false;
        }
        if self.extensions.is_empty() || key.starts_with("link:") {
            return true;
        }
        let Some((name, version)) = pin::package_key(key).rsplit_once('@') else {
            return false;
        };
        let Some(rules) = self.extensions.get(name) else {
            return true;
        };
        // Preserve declaration order: pnpm applies the first extension's field
        // values before later matching extensions fill in missing fields.
        let applicable =
            |rule: &&(&str, &Value)| crate::schema::parent_matches(rule.0, name, version);
        rules[0]
            .iter()
            .filter(applicable)
            .map(|(_, value)| value)
            .eq(rules[1].iter().filter(applicable).map(|(_, value)| value))
    }
}
