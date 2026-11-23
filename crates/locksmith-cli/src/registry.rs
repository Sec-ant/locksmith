//! Metadata transport and registry selection. Constructed only after a local miss.
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use jiff::Timestamp;
use locksmith::{RegistryMetadata, RegistryRequest};
use serde::Deserialize;
use serde_json::Value;
use ureq::{Agent, http::Uri};

use crate::Result;
mod generation;
pub use generation::GenerationSource;
mod metadata;
mod publication;
pub(crate) use publication::RESOLUTION_FIELDS;
mod selection;
mod settings;
mod trust;
use settings::{apply_auth, copy_settings, env_config, project_rc};

const JSR_REGISTRY: &str = "https://npm.jsr.io/";

#[derive(Clone, Copy)]
pub enum ProjectRc<'a> {
    Working,
    Git {
        settings: Option<&'a str>,
        credentials: Option<&'a str>,
    },
}

#[derive(Deserialize, serde::Serialize)]
struct Packument<Release = Box<serde_json::value::RawValue>> {
    #[serde(default, rename = "_locksmithFreshUntil")]
    fresh_until: i64,
    #[serde(
        default,
        rename = "_locksmithETag",
        skip_serializing_if = "Option::is_none"
    )]
    etag: Option<String>,
    name: String,
    versions: BTreeMap<String, Release>,
    #[serde(rename = "dist-tags", default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    time: BTreeMap<String, Value>,
    #[serde(skip)]
    downgrades: std::cell::OnceCell<BTreeSet<String>>,
}

struct Registry {
    agent: Agent,
    network_concurrency: usize,
    rc: BTreeMap<String, String>,
    config: Value,
    packages: BTreeMap<(String, String), Packument>,
    pinned: BTreeMap<(String, String), Packument<u8>>,
    failed: BTreeMap<(String, String), String>,
    global_auth_routes: BTreeMap<String, String>,
    env_auth_routes: BTreeMap<String, String>,
    min_age: u64,
    now: i64,
    generation_network: Option<bool>,
    transitive_cutoff: Option<i64>,
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display()).into()),
    }
}

fn extend(rc: &mut BTreeMap<String, String>, text: &str, accept: impl Fn(&str) -> bool) {
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with([';', '#']))
    {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if accept(key) {
                rc.insert(
                    key.to_owned(),
                    value.trim().trim_matches(['"', '\'']).to_owned(),
                );
            }
        }
    }
}

fn credential(k: &str) -> bool {
    k.starts_with("//")
        && k.rsplit_once(':')
            .is_some_and(|(_, k)| matches!(k, "_authToken" | "_auth" | "_password" | "username"))
}

fn expand_env(value: &str) -> Result<String> {
    let mut rest = value;
    let mut out = String::new();
    while let Some((prefix, next)) = rest.split_once("${") {
        let (name, suffix) = next
            .split_once('}')
            .ok_or("invalid registry environment placeholder")?;
        out.push_str(prefix);
        out.push_str(
            &env::var(name).map_err(|_| format!("missing registry environment variable {name}"))?,
        );
        rest = suffix;
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    Merge,
    Converge,
}

pub fn resolve(
    purpose: Lookup,
    network: Option<ProjectRc<'_>>,
    config: &Value,
    root: &Value,
    mut solve: impl FnMut(
        Option<&RegistryMetadata>,
    ) -> std::result::Result<locksmith::Lockfile, locksmith::LockfileError>,
) -> Result<locksmith::Lockfile> {
    let Some(project) = network else {
        return Ok(solve(None)?);
    };
    let mut data = RegistryMetadata::default();
    let mut registry = None;
    let mut supplied = BTreeSet::new();
    loop {
        match solve(Some(&data)) {
            Err(locksmith::LockfileError::ResolutionRequired {
                request: Some(ref request),
                ref path,
                ..
            }) if supplied.insert(request.clone()) => {
                let fetched = (|| {
                    let client = match &mut registry {
                        Some(client) => client,
                        None => registry.insert(Registry::new(
                            config,
                            crate::workspace::pnpm_version(root).map(|v| v.major()),
                            project,
                        )?),
                    };
                    client.satisfy(
                        request,
                        &mut data,
                        path.first().is_some_and(|s| s == "importers"),
                    )
                })();
                if let Err(error) = fetched {
                    if purpose == Lookup::Converge
                        && let RegistryRequest::Manifest { name, version } = request.as_ref()
                    {
                        data.skip_manifest(name, version);
                        eprintln!(
                            "locksmith: keeping installed dependency pins for {name}@{version}: {error}"
                        );
                    } else {
                        return Err(locksmith::LockfileError::ResolutionRequired {
                            path: path.clone(),
                            message: error.to_string(),
                            request: Some(request.clone()),
                        }
                        .into());
                    }
                }
            }
            other => return Ok(other?),
        }
    }
}

impl Registry {
    fn new(workspace: &Value, major: Option<u64>, project: ProjectRc<'_>) -> Result<Self> {
        let mut rc = BTreeMap::new();
        let home = env::var_os("HOME").map(PathBuf::from);
        let user = env::var_os("PNPM_CONFIG_NPMRC_AUTH_FILE")
            .or_else(|| env::var_os("pnpm_config_npmrc_auth_file"))
            .or_else(|| env::var_os("npm_config_userconfig"))
            .or_else(|| env::var_os("NPM_CONFIG_USERCONFIG"))
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|p| p.join(".npmrc")));
        let cfg_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                home.as_ref().map(|p| {
                    p.join(if cfg!(target_os = "macos") {
                        "Library/Preferences"
                    } else {
                        ".config"
                    })
                })
            });
        let mut config = serde_json::json!({});
        let mut global_auth_routes = BTreeMap::new();
        let dir = cfg_home.map(|dir| dir.join("pnpm"));
        if let Some(dir) = &dir
            && let Some(text) = read(&dir.join("rc"))?
        {
            extend(&mut rc, &text, |_| true);
        }
        if let Some(user) = user
            && let Some(text) = read(&user)?
        {
            extend(&mut rc, &text, |_| true);
        }
        if let Some(dir) = dir {
            if let Some(text) = read(&dir.join("auth.ini"))? {
                extend(&mut rc, &text, credential);
            }
            if let Some(text) = read(&dir.join("config.yaml"))? {
                let global: Option<Value> = serde_saphyr::from_str(&text)?;
                if let Some(global) = global {
                    copy_settings(&mut config, &global);
                    if let Some(auth) = global.get("_auth") {
                        global_auth_routes = apply_auth(&mut rc, auth)?;
                    }
                }
            }
        }
        copy_settings(&mut config, workspace);
        let settings;
        match project {
            ProjectRc::Working => {
                settings = read(Path::new(".npmrc"))?;
                if let Some(text) = settings.as_deref() {
                    project_rc(&mut rc, text, true);
                }
            }
            ProjectRc::Git {
                settings,
                credentials,
            } => {
                if let Some(text) = credentials {
                    for line in text.lines().filter(|line| {
                        line.split_once('=')
                            .is_some_and(|(key, _)| credential(key.trim()))
                    }) {
                        project_rc(&mut rc, line, true);
                    }
                }
                if let Some(text) = settings {
                    project_rc(&mut rc, text, false);
                }
            }
        }
        let env_auth_routes = match env_config("pnpm", "_auth") {
            Some(text) => apply_auth(&mut rc, &serde_json::from_str(&text)?)?,
            None => BTreeMap::new(),
        };
        for (key, field) in [
            ("network_concurrency", "networkConcurrency"),
            ("minimum_release_age", "minimumReleaseAge"),
            ("minimum_release_age_strict", "minimumReleaseAgeStrict"),
            ("minimum_release_age_exclude", "minimumReleaseAgeExclude"),
            (
                "minimum_release_age_ignore_missing_time",
                "minimumReleaseAgeIgnoreMissingTime",
            ),
            ("resolution_mode", "resolutionMode"),
            ("trust_policy", "trustPolicy"),
            ("trust_policy_ignore_after", "trustPolicyIgnoreAfter"),
            ("trust_policy_exclude", "trustPolicyExclude"),
            ("lockfile_include_tarball_url", "lockfileIncludeTarballUrl"),
        ] {
            let value = env_config("pnpm", key).or_else(|| env_config("npm", key));
            let legacy = key.replace('_', "-");
            if let Some(v) = value.as_deref().or_else(|| {
                config
                    .get(field)
                    .is_none()
                    .then(|| rc.get(&legacy).map(String::as_str))
                    .flatten()
            }) {
                config[field] = serde_json::from_str(v).unwrap_or_else(|_| Value::from(v));
            }
        }
        // pnpm 11 skips undated metadata unless the project opts out.
        if major.is_some_and(|v| v >= 11)
            && config.get("minimumReleaseAgeIgnoreMissingTime").is_none()
        {
            config["minimumReleaseAgeIgnoreMissingTime"] = true.into();
        }
        let min_age = match config.get("minimumReleaseAge") {
            Some(v) => v
                .as_u64()
                .ok_or("minimumReleaseAge must be a nonnegative number of minutes")?,
            None => rc
                .get("minimum-release-age")
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(if major.is_some_and(|v| v >= 11) {
                    1440
                } else {
                    0
                }),
        };
        // pnpm's package requester sizes its connection pool for registry
        // latency. Honor the project's override for shared or local registries.
        let parallelism = std::thread::available_parallelism().map_or(1, |n| n.get());
        let network_concurrency = match config.get("networkConcurrency") {
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|&n| n > 0)
                .ok_or("networkConcurrency must be a positive integer")?,
            None => parallelism
                .saturating_sub(1)
                .saturating_mul(3)
                .clamp(64, 96),
        };
        let mut builder = Agent::config_builder()
            .max_idle_connections(network_concurrency)
            .max_idle_connections_per_host(network_concurrency)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)));
        if let Some(path) =
            env::var_os("NPM_CONFIG_CAFILE").or_else(|| rc.get("cafile").map(Into::into))
        {
            let bytes = fs::read(path)?;
            let mut certs = Vec::new();
            for item in ureq::tls::parse_pem(&bytes) {
                if let ureq::tls::PemItem::Certificate(cert) = item? {
                    certs.push(cert);
                }
            }
            if certs.is_empty() {
                return Err("registry cafile contains no certificates".into());
            }
            builder = builder.tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
                    .build(),
            );
        }
        Ok(Self {
            agent: Agent::new_with_config(builder.build()),
            network_concurrency,
            config,
            rc,
            packages: BTreeMap::new(),
            pinned: BTreeMap::new(),
            failed: BTreeMap::new(),
            global_auth_routes,
            env_auth_routes,
            min_age,
            now: Timestamp::now().as_second(),
            generation_network: None,
            transitive_cutoff: None,
        })
    }

    fn registry(&self, name: &str) -> Result<String> {
        let scope = name.split_once('/').map(|(scope, _)| scope);
        if let Some(routes) = self.config.get("registriesByScope") {
            let base = scope
                .and_then(|scope| routes.get(scope))
                .unwrap_or(&routes["default"])
                .as_str()
                .ok_or("registriesByScope must contain registry URLs")?;
            return Self::validated_base(base);
        }
        let mut routes = BTreeMap::new();
        if let Some(registries) = self.config.get("registries") {
            for (key, entry) in registries
                .as_object()
                .ok_or("registries must be a mapping")?
            {
                if let Some(url) = entry.as_str() {
                    routes.insert(key.as_str(), (url, false));
                } else {
                    let scopes = entry
                        .get("scopes")
                        .map(|v| v.as_array().ok_or("registry scopes must be a list"))
                        .transpose()?;
                    for route in scopes.into_iter().flatten() {
                        let route = route.as_str().ok_or("invalid registry scope")?;
                        let route = if route == "@" { "default" } else { route };
                        if routes
                            .insert(route, (key, entry.get("prefix").is_some()))
                            .is_some()
                        {
                            return Err(
                                format!("multiple registries configured for {route}").into()
                            );
                        }
                    }
                }
            }
        }
        let scoped = scope.and_then(|scope| {
            routes.get(scope).copied().or_else(|| {
                self.rc
                    .get(&format!("{scope}:registry"))
                    .map(|url| (url.as_str(), false))
            })
        });
        let registry_env = env_config("pnpm", "registry").or_else(|| env_config("npm", "registry"));
        let environment_route = scope
            .and_then(|scope| self.env_auth_routes.get(scope))
            .or_else(|| self.env_auth_routes.get("default"));
        let (base, qualified) = environment_route
            .map(|v| (v.as_str(), false))
            .or(scoped)
            .or_else(|| {
                scope
                    .and_then(|s| self.global_auth_routes.get(s))
                    .map(|v| (v.as_str(), false))
            })
            .or_else(|| registry_env.as_deref().map(|v| (v, false)))
            .or_else(|| routes.get("default").copied())
            .or_else(|| self.config["registry"].as_str().map(|v| (v, false)))
            .or_else(|| self.rc.get("registry").map(|v| (v.as_str(), false)))
            .or_else(|| {
                self.global_auth_routes
                    .get("default")
                    .map(|v| (v.as_str(), false))
            })
            .unwrap_or(("https://registry.npmjs.org/", false));
        if qualified && self.generation_network.is_none() {
            return Err(format!("registry-qualified lock identity required for {name}").into());
        }
        Self::validated_base(base)
    }

    fn validated_base(base: &str) -> Result<String> {
        let uri: Uri = base.parse().map_err(|_| "invalid registry URL")?;
        if !matches!(uri.scheme_str(), Some("https" | "http"))
            || uri.authority().is_none_or(|a| a.as_str().contains('@'))
            || uri.query().is_some()
            || base.contains(['#', '$'])
        {
            return Err("expected an HTTP(S) registry URL without credentials, placeholders, query or fragment".into());
        }
        Ok(format!("{}/", base.trim_end_matches('/')))
    }

    fn auth(&self, url: &str, name: &str) -> Result<Option<String>> {
        let uri: Uri = url.parse()?;
        let mut scope = format!(
            "//{}{}",
            uri.authority().ok_or("missing registry host")?,
            uri.path()
        );
        while let Some(end) = scope.rfind('/') {
            if end < 2 {
                break;
            }
            scope.truncate(end + 1);
            let field = |key| {
                name.split_once('/')
                    .and_then(|(org, _)| self.rc.get(&format!("{scope}:{org}:{key}")))
                    .or_else(|| self.rc.get(&format!("{scope}:{key}")))
            };
            if let Some(token) = field("_authToken") {
                return Ok(Some(format!("Bearer {}", expand_env(token)?)));
            }
            if let Some(value) = field("_auth") {
                return Ok(Some(format!("Basic {}", expand_env(value)?)));
            }
            if let (Some(user), Some(pass)) = (field("username"), field("_password")) {
                let mut bytes = expand_env(user)?.into_bytes();
                bytes.push(b':');
                bytes.extend(STANDARD.decode(expand_env(pass)?)?);
                return Ok(Some(format!("Basic {}", STANDARD.encode(bytes))));
            }
            scope.pop();
        }
        Ok(None)
    }

    fn load(&mut self, name: &str) -> Result<String> {
        let base = self.registry(name)?;
        self.load_at(name, &base)?;
        Ok(base)
    }

    fn load_at(&mut self, name: &str, base: &str) -> Result<()> {
        let key = (base.to_owned(), name.to_owned());
        if let Some(message) = self.failed.get(&key) {
            return Err(message.clone().into());
        }
        if !self.packages.contains_key(&key) {
            let request = self.metadata_request(name, base)?;
            let doc = request.read(&self.agent)?;
            self.packages.insert(key, doc);
        }
        Ok(())
    }

    fn satisfy(
        &mut self,
        req: &RegistryRequest,
        data: &mut RegistryMetadata,
        direct: bool,
    ) -> Result<()> {
        let name = req.name();
        let base = self.load(name)?;
        let doc = &self.packages[&(base.clone(), name.to_owned())];
        let selected = match req {
            RegistryRequest::Manifest { version, .. } => version.as_str(),
            RegistryRequest::Version { selector, .. } => {
                self.select(doc, selector, direct)?.ok_or_else(|| {
                    format!("no eligible registry version of {name} satisfies {selector}")
                })?
            }
        };
        let raw = doc
            .versions
            .get(selected)
            .ok_or_else(|| format!("registry has no release {name}@{selected}"))?;
        data.insert(
            req,
            serde_json::from_str(raw.get())?,
            &base,
            self.config["lockfileIncludeTarballUrl"]
                .as_bool()
                .unwrap_or(false),
        )?;
        Ok(())
    }
}

/// Public scope routing passed to hooks. Credentials stay in the registry client.
pub(crate) fn hook_registries(config: &Value, major: u64) -> Result<Value> {
    if let Some(routes) = config.get("registriesByScope") {
        return Ok(routes.clone());
    }
    let mut registry = Registry::new(config, Some(major), ProjectRc::Working)?;
    registry.generation_network = Some(false);
    let mut scopes = std::collections::BTreeSet::<&str>::new();
    scopes.extend(
        registry
            .rc
            .keys()
            .filter_map(|k| k.strip_suffix(":registry")),
    );
    scopes.extend(
        registry
            .env_auth_routes
            .keys()
            .chain(registry.global_auth_routes.keys())
            .map(String::as_str),
    );
    for (key, entry) in registry.config["registries"]
        .as_object()
        .into_iter()
        .flatten()
    {
        if entry.is_string() {
            scopes.insert(key);
        }
        scopes.extend(
            entry["scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
    }
    let mut routes = serde_json::json!({"default":registry.registry("")?, "@jsr":JSR_REGISTRY});
    for scope in scopes
        .into_iter()
        .filter(|s| s.starts_with('@') && *s != "@")
    {
        routes[scope] = registry.registry(&format!("{scope}/package"))?.into();
    }
    Ok(routes)
}

pub(crate) fn updated_hook_registries(mut routes: Value, default: &Value) -> Result<Value> {
    let map = routes
        .as_object_mut()
        .ok_or("updateConfig registriesByScope must be a mapping")?;
    // A returned map replaces the scope routes. Keep the original fallback
    // and the built-in JSR route when the hook omits them.
    map.entry("default").or_insert_with(|| default.clone());
    map.entry("@jsr").or_insert_with(|| JSR_REGISTRY.into());
    for url in map.values_mut() {
        let base = url
            .as_str()
            .ok_or("updateConfig registriesByScope must contain registry URLs")?;
        *url = Registry::validated_base(base)?.into();
    }
    Ok(routes)
}
