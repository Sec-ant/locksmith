mod sources;

use locksmith::{LockfileError, PackageSource, RegistryPackage, SourceManifest};
use node_semver::{Range, Version};
use serde_json::Value;

use super::{JSR_REGISTRY, ProjectRc, Registry};

pub struct GenerationSource<'a> {
    config: &'a Value,
    major: Option<u64>,
    network: bool,
    hook_manifests: bool,
    prepared: std::collections::BTreeMap<(RegistryPackage, String), Value>,
    registry: Option<Registry>,
    routes: std::collections::BTreeMap<RegistryPackage, String>,
}
impl<'a> GenerationSource<'a> {
    pub fn new(config: &'a Value, major: Option<u64>, network: bool, hook_manifests: bool) -> Self {
        Self {
            config,
            major,
            network,
            hook_manifests,
            prepared: Default::default(),
            registry: None,
            routes: Default::default(),
        }
    }
    fn client(&mut self) -> crate::Result<&mut Registry> {
        if self.registry.is_none() {
            let mut registry = Registry::new(self.config, self.major, ProjectRc::Working)?;
            registry.generation_network = Some(self.network);
            self.registry = Some(registry);
        }
        Ok(self.registry.as_mut().expect("initialized registry"))
    }
    fn route(
        &mut self,
        name: &str,
        spec: &str,
    ) -> crate::Result<Option<(RegistryPackage, String)>> {
        let built_in = self.major.is_none_or(|v| v >= 11);
        let registry = self.client()?;
        let named = named_registries(&registry.config, built_in)?;
        let (prefix, selector) = spec.split_once(':').unwrap_or(("", spec));
        let explicit = named.get(prefix).map(String::as_str);
        let request = if prefix == "jsr" || explicit.is_some() {
            let (actual, range) = selector
                .rsplit_once('@')
                .filter(|(n, _)| !n.is_empty())
                .unwrap_or((name, selector));
            let actual = if prefix == "jsr" {
                format!("@jsr/{}", actual.trim_start_matches('@').replace('/', "__"))
            } else {
                actual.into()
            };
            RegistryPackage::selector(&actual, range)
        } else {
            RegistryPackage::selector(name, spec)
        };
        let Some((mut package, selector)) = request else {
            return Ok(None);
        };
        let mut base = match explicit {
            Some(url) => Registry::validated_base(url)?,
            None => registry.registry(&package.name)?,
        };
        let default = registry.registry("")?;
        if prefix == "jsr"
            && base == default
            && !registry.rc.contains_key("@jsr:registry")
            && registry.config["registries"].get("@jsr").is_none()
            && registry.config.get("registriesByScope").is_none()
        {
            base = JSR_REGISTRY.into();
        }
        if base != default {
            // An alias in the specifier names its registry. A plain dependency
            // routed to another registry takes a declared prefix for that URL.
            package.prefix = if explicit.is_some() {
                Some(prefix.to_owned())
            } else {
                named_registries(&registry.config, false)?
                    .into_iter()
                    .find(|(_, url)| url.trim_end_matches('/') == base.trim_end_matches('/'))
                    .map(|(p, _)| p)
            };
        }
        if self
            .routes
            .insert(package.clone(), base.clone())
            .is_some_and(|old| old != base)
        {
            return Err(format!(
                "two registries give {} the same lockfile identity",
                package.name
            )
            .into());
        }
        Ok(Some((package, selector)))
    }
    fn package_base(&mut self, package: &RegistryPackage) -> crate::Result<String> {
        let route = self.routes.get(package).cloned();
        let built_in = self.major.is_none_or(|v| v >= 11);
        let client = self.client()?;
        Ok(match route {
            Some(url) => url,
            None => match &package.prefix {
                Some(prefix) => {
                    let named = named_registries(&client.config, built_in)?;
                    let url = named
                        .get(prefix)
                        .ok_or_else(|| format!("missing registry {prefix}"))?;
                    Registry::validated_base(url)?
                }
                None => client.registry(&package.name)?,
            },
        })
    }
    fn load(&mut self, package: &RegistryPackage) -> crate::Result<(&Registry, (String, String))> {
        let base = self.package_base(package)?;
        let client = self.client()?;
        client.load_at(&package.name, &base)?;
        Ok((client, (base, package.name.clone())))
    }
}

/// Registry aliases by prefix. A `registries` declaration takes a prefix from a
/// `namedRegistries` entry, which overrides pnpm's built-in `gh` and `npmjs`.
fn named_registries(
    config: &Value,
    built_in: bool,
) -> crate::Result<std::collections::BTreeMap<String, String>> {
    let mut named = std::collections::BTreeMap::new();
    if built_in {
        named.insert("gh".to_owned(), "https://npm.pkg.github.com/".to_owned());
        named.insert("npmjs".to_owned(), "https://registry.npmjs.org/".to_owned());
    }
    for (prefix, url) in config["namedRegistries"].as_object().into_iter().flatten() {
        let url = url.as_str().ok_or("expected named registry URL")?;
        named.insert(prefix.clone(), url.to_owned());
    }
    for (url, entry) in config["registries"].as_object().into_iter().flatten() {
        if let Some(prefix) = entry["prefix"].as_str() {
            named.insert(prefix.to_owned(), url.clone());
        }
    }
    Ok(named)
}
fn error(path: &[&str], error: impl ToString) -> LockfileError {
    LockfileError::ResolutionRequired {
        path: path.iter().map(|s| s.to_string()).collect(),
        message: error.to_string(),
        request: None,
    }
}
impl PackageSource for GenerationSource<'_> {
    fn prepare_releases(
        &mut self,
        releases: &std::collections::BTreeSet<(RegistryPackage, String)>,
    ) -> Result<(), LockfileError> {
        if !self.hook_manifests || releases.is_empty() {
            return Ok(());
        }
        let mut work = Vec::new();
        for (pkg, version) in releases {
            let key = (pkg.clone(), version.clone());
            if self.prepared.contains_key(&key) {
                continue;
            }
            let base = self
                .package_base(pkg)
                .map_err(|e| error(&[&pkg.name, version], e))?;
            let request = self
                .client()
                .and_then(|c| c.hook_request(&pkg.name, version, &base))
                .map_err(|e| error(&[&pkg.name, version], e))?;
            work.push((key, request));
        }
        let fetched = self
            .client()
            .map_err(|e| error(&["packages"], e))?
            .read_metadata(work, |request, key, agent| request.manifest(agent, &key.1));
        for (key, value) in fetched {
            let value = value.map_err(|e| error(&[&key.0.name, &key.1], e))?;
            self.prepared.insert(key, value);
        }
        Ok(())
    }
    fn prepare_pins(
        &mut self,
        packages: &std::collections::BTreeSet<RegistryPackage>,
    ) -> Result<(), LockfileError> {
        if packages.is_empty() {
            return Ok(());
        }
        let mut prepare = || -> crate::Result<()> {
            let client = self.client()?;
            if client.config["trustLockfile"] == true
                || (client.config["trustPolicy"]
                    .as_str()
                    .is_none_or(|s| s == "off")
                    && client.min_age == 0
                    && client.config["resolutionMode"] != "time-based")
            {
                return Ok(());
            }
            let keys = packages
                .iter()
                .map(|p| Ok((self.package_base(p)?, p.name.clone())))
                .collect::<crate::Result<Vec<_>>>()?;
            self.client()?.warm(keys)
        };
        prepare().map_err(|e| error(&["packages"], e))
    }
    fn registry_selector(
        &mut self,
        name: &str,
        spec: &str,
    ) -> Result<Option<(RegistryPackage, String)>, LockfileError> {
        self.route(name, spec)
            .map_err(|err| error(&[name, spec], err))
    }
    fn versions(
        &mut self,
        package: &RegistryPackage,
        selector: &str,
        direct: bool,
    ) -> Result<Vec<String>, LockfileError> {
        let mut query = || -> crate::Result<Vec<String>> {
            let (registry, key) = self.load(package)?;
            let doc = &registry.packages[&key];
            let Some(preferred) = registry.select(doc, selector, direct)? else {
                return Ok(Vec::new());
            };
            let mut versions = vec![preferred.to_owned()];
            if let Ok(range) = selector.parse::<Range>() {
                let mut choices = Vec::new();
                let age_filter = registry.min_age > 0 && registry.old_enough(doc, preferred)?;
                // A time-based fallback outside the cutoff also lifts it for alternatives.
                let cutoff = registry.before_cutoff(doc, preferred, direct)?;
                for version in doc.versions.keys().filter(|v| v.as_str() != preferred) {
                    if let Ok(parsed) = version.parse::<Version>()
                        && parsed.satisfies(&range)
                        && (!age_filter || registry.old_enough(doc, version)?)
                        && (!cutoff || registry.before_cutoff(doc, version, direct)?)
                        && registry.trusted(doc, version)?
                    {
                        choices.push((parsed, version));
                    }
                }
                choices.sort_unstable_by(|(a, _), (b, _)| {
                    if direct
                        && matches!(
                            registry.config["resolutionMode"].as_str(),
                            Some("lowest-direct" | "time-based")
                        )
                    {
                        a.cmp(b)
                    } else {
                        b.cmp(a)
                    }
                });
                versions.extend(choices.into_iter().map(|(_, v)| v.clone()));
            }
            Ok(versions)
        };
        query().map_err(|e| error(&[&package.name, selector], e))
    }
    fn release(
        &mut self,
        package: &RegistryPackage,
        version: &str,
    ) -> Result<SourceManifest, LockfileError> {
        let mut fetch = || -> crate::Result<SourceManifest> {
            let name = &package.name;
            let base = self.package_base(package)?;
            let include_tarball = self.client()?.config["lockfileIncludeTarballUrl"] == true;
            let raw = if let Some(value) = self.prepared.remove(&(package.clone(), version.into()))
            {
                value
            } else if self.hook_manifests {
                let client = self.client()?;
                client
                    .hook_request(name, version, &base)?
                    .manifest(&client.agent, version)?
            } else {
                let (client, key) = self.load(package)?;
                let publication = client.packages[&key]
                    .versions
                    .get(version)
                    .ok_or_else(|| format!("no registry release {name}@{version}"))?;
                serde_json::from_str(publication.get())?
            };
            let mut release = SourceManifest::registry(name, version, raw, &base, include_tarball)?;
            release.key = package.key(version);
            Ok(release)
        };
        fetch().map_err(|e| error(&[&package.name, version], e))
    }
    fn accepts_pin(
        &mut self,
        package: &RegistryPackage,
        version: &str,
        direct: bool,
    ) -> Result<bool, LockfileError> {
        let mut check = || -> crate::Result<bool> {
            let client = self.client()?;
            if client.config["trustLockfile"] == true
                || (client.config["trustPolicy"]
                    .as_str()
                    .is_none_or(|s| s == "off")
                    && client.min_age == 0
                    && (direct || client.transitive_cutoff.is_none()))
            {
                return Ok(true);
            }
            let base = self.package_base(package)?;
            let client = self.client()?;
            let key = (base, package.name.clone());
            if let Some(doc) = client.packages.get(&key) {
                return client.accepts_locked(doc, version, direct);
            }
            if let Some(index) = client.pinned.get(&key) {
                return client.accepts_locked(index, version, direct);
            }
            client.load_at(&key.1, &key.0)?;
            client.accepts_locked(&client.packages[&key], version, direct)
        };
        check().map_err(|e| error(&[&package.name, version], e))
    }
    fn direct_times(
        &mut self,
        direct: &std::collections::BTreeSet<(RegistryPackage, String)>,
        recorded: &Value,
    ) -> Result<Option<std::collections::BTreeMap<String, String>>, LockfileError> {
        let mut query = || -> crate::Result<_> {
            if self.client()?.config["resolutionMode"] != "time-based" {
                return Ok(None);
            }
            let mut times = std::collections::BTreeMap::new();
            let mut newest = None;
            for (package, v) in direct {
                let key = package.key(v);
                let stamp = if let Some(t) = recorded
                    .get(&key)
                    .or_else(|| recorded.get(format!("/{key}")))
                    .and_then(Value::as_str)
                {
                    t.to_owned()
                } else {
                    let (client, key) = self.load(package)?;
                    client.packages[&key]
                        .time
                        .get(v)
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("missing publish time for {}@{v}", package.name))?
                        .to_owned()
                };
                newest = newest.max(Some(stamp.parse::<jiff::Timestamp>()?.as_millisecond()));
                times.insert(key, stamp);
            }
            // The cutoff and publish times keep their milliseconds.
            self.client()?.transitive_cutoff = newest.map(|t| t.saturating_add(3_600_000));
            Ok(Some(times))
        };
        query().map_err(|e| error(&["time"], e))
    }
    fn other(
        &mut self,
        from: &str,
        name: &str,
        spec: &str,
    ) -> Result<SourceManifest, LockfileError> {
        sources::resolve(self, from, name, spec).map_err(|err| error(&[from, name], err))
    }
}

pub(super) fn cache_file(
    url: &str,
    accept: &str,
    auth: Option<&str>,
) -> Option<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
            Some(home.join(if cfg!(target_os = "macos") {
                "Library/Caches"
            } else {
                ".cache"
            }))
        })?;
    let mut hash = Sha256::new();
    for value in [url, accept, auth.unwrap_or_default()] {
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    Some(
        dir.join("locksmith/registry")
            .join(format!("{:x}.json", hash.finalize())),
    )
}
