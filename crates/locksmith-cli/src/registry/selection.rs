use super::{Packument, Registry};
use crate::Result;
use jiff::Timestamp;
use node_semver::{Range, Version};
use serde::Deserialize;
use serde_json::Value;

fn deprecated(publication: &serde_json::value::RawValue) -> Result<bool> {
    #[derive(Deserialize)]
    struct Status {
        deprecated: Option<Value>,
    }
    match serde_json::from_str::<Status>(publication.get())?.deprecated {
        None => Ok(false),
        Some(Value::Bool(flag)) => Ok(flag),
        Some(Value::String(message)) => Ok(!message.is_empty()),
        Some(_) => Err("expected a deprecation message or boolean in registry metadata".into()),
    }
}

fn excluded(policy: &Value, name: &str, version: &str) -> Result<bool> {
    if policy.is_null() {
        return Ok(false);
    }
    for item in policy
        .as_array()
        .ok_or("registry policy exclusions must be a list")?
    {
        let pattern = item
            .as_str()
            .ok_or("expected a package selector in registry policy")?;
        let (package, versions) = pattern
            .rsplit_once('@')
            .filter(|(package, _)| !package.is_empty())
            .map_or((pattern, None), |(package, range)| (package, Some(range)));
        if glob::Pattern::new(package)?.matches(name)
            && versions.is_none_or(|v| v.split("||").any(|v| v.trim() == version))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Registry {
    fn old_enough(&self, doc: &Packument, version: &str) -> Result<bool> {
        if self.min_age == 0
            || excluded(
                &self.settings.config["minimumReleaseAgeExclude"],
                &doc.name,
                version,
            )?
        {
            return Ok(true);
        }
        let Some(date) = doc.time.get(version).and_then(Value::as_str) else {
            if self.settings.config["minimumReleaseAgeIgnoreMissingTime"] == true {
                return Ok(true);
            }
            return Err(format!("missing publish time for {}@{version}", doc.name).into());
        };
        let seconds = i64::try_from(self.min_age.saturating_mul(60)).unwrap_or(i64::MAX);
        Ok(date.parse::<Timestamp>()?.as_second() <= self.now.saturating_sub(seconds))
    }

    pub(super) fn select<'p>(
        &self,
        doc: &'p Packument,
        specifier: &str,
        direct: bool,
    ) -> Result<&'p str> {
        let mode = self.settings.config["resolutionMode"]
            .as_str()
            .unwrap_or("highest");
        if !matches!(mode, "highest" | "lowest-direct") {
            return Err(format!(
                "registry resolutionMode {mode} requires additional resolution context"
            )
            .into());
        }
        let range = specifier.parse::<Range>().ok();
        let lowest = direct && mode == "lowest-direct" && range.is_some();
        let pick = |age_filter: bool| -> Result<Option<&'p str>> {
            let eligible =
                |v: &str| -> Result<bool> { Ok(!age_filter || self.old_enough(doc, v)?) };
            let tag = if range.is_some() { "latest" } else { specifier };
            if !lowest
                && let Some(version) = doc.tags.get(tag)
                && doc.versions.contains_key(version)
                && (specifier == "*"
                    || range
                        .as_ref()
                        .is_none_or(|r| version.parse::<Version>().is_ok_and(|v| v.satisfies(r))))
                && eligible(version)?
            {
                return Ok(Some(version));
            }
            let Some(range) = &range else { return Ok(None) };
            // The common path only inspects version keys. Decode the deprecated
            // field if the best available release carries a deprecation notice.
            let best = |skip_deprecated: bool| -> Result<Option<&'p str>> {
                let mut result: Option<(Version, &str)> = None;
                for (version, manifest) in &doc.versions {
                    let Ok(parsed) = version.parse::<Version>() else {
                        continue;
                    };
                    if !(parsed.satisfies(range) || lowest && specifier == "*")
                        || result.as_ref().is_some_and(|(old, _)| {
                            if lowest {
                                parsed >= *old
                            } else {
                                parsed <= *old
                            }
                        })
                        || !eligible(version)?
                    {
                        continue;
                    }
                    if skip_deprecated && deprecated(manifest)? {
                        continue;
                    }
                    result = Some((parsed, version));
                }
                Ok(result.map(|(_, version)| version))
            };
            let chosen = best(false)?;
            if let Some(version) = chosen
                && !lowest
                && deprecated(&doc.versions[version])?
            {
                return Ok(best(true)?.or(chosen));
            }
            Ok(chosen)
        };
        let mut selected = pick(self.min_age > 0)?;
        let strict = self.settings.config["minimumReleaseAgeStrict"]
            .as_bool()
            .unwrap_or(self.settings.config.get("minimumReleaseAge").is_some());
        if selected.is_none() && self.min_age > 0 && !strict {
            selected = pick(false)?;
        }
        let selected = selected.ok_or_else(|| {
            format!(
                "no eligible registry version of {} satisfies {specifier}",
                doc.name
            )
        })?;
        if let Some(policy) = self.settings.config["trustPolicy"].as_str()
            && policy != "off"
            && !excluded(
                &self.settings.config["trustPolicyExclude"],
                &doc.name,
                selected,
            )?
        {
            return Err(format!(
                "{}@{selected} requires publication trust evidence for trustPolicy {policy}",
                doc.name
            )
            .into());
        }
        Ok(selected)
    }
}
