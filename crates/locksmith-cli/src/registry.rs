//! Metadata transport and registry selection. Constructed only after a local miss.
use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufReader, ErrorKind},
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use jiff::Timestamp;
use locksmith::{RegistryMetadata, RegistryRequest};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use ureq::{Agent, http::Uri};

use crate::Result;
mod selection;
mod settings;
use settings::{apply_auth, copy_settings, env_config, project_rc};

#[derive(Clone, Copy)]
pub enum ProjectRc<'a> {
    Working,
    Git {
        settings: Option<&'a str>,
        credentials: Option<&'a str>,
    },
}

#[derive(Deserialize)]
struct Packument {
    name: String,
    versions: BTreeMap<String, Box<RawValue>>,
    #[serde(rename = "dist-tags", default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    time: BTreeMap<String, Value>,
}

struct Registry {
    agent: Agent,
    settings: RegistrySettings,
    packages: BTreeMap<String, std::result::Result<Packument, String>>,
    min_age: u64,
    now: i64,
}

struct RegistrySettings {
    rc: BTreeMap<String, String>,
    config: Value,
    global_auth_routes: BTreeMap<String, String>,
    env_auth_routes: BTreeMap<String, String>,
}

/// Resolve only the registry routes needed to verify explicitly declared bootstrap tarballs.
pub fn bootstrap_registries(
    workspace: &Value,
    read_project_rc: impl FnOnce() -> Result<Option<String>>,
) -> Result<BTreeMap<String, String>> {
    let mut routes = BTreeMap::new();
    let Some(deps) = workspace["configDependencies"].as_object() else {
        return Ok(routes);
    };
    let mut names = deps
        .iter()
        .filter(|(_, dep)| dep.get("tarball").is_some())
        .peekable();
    if names.peek().is_some() {
        let rc = read_project_rc()?;
        let settings = RegistrySettings::load(
            workspace,
            ProjectRc::Git {
                settings: rc.as_deref(),
                credentials: None,
            },
        )?;
        for (name, _) in names {
            let scope = name.split_once('/').map_or("default", |(scope, _)| scope);
            if !routes.contains_key(scope) {
                routes.insert(scope.to_owned(), settings.registry(name)?);
            }
        }
    }
    Ok(routes)
}

pub fn working_rc() -> Result<Option<String>> {
    read(Path::new(".npmrc"))
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
    pnpm_major: Option<u64>,
    mut solve: impl FnMut(
        Option<&RegistryMetadata>,
    )
        -> std::result::Result<locksmith::Lockfile, Vec<locksmith::LockfileError>>,
) -> Result<locksmith::Lockfile> {
    let Some(project) = network else {
        return Ok(
            solve(None).map_err(|errors| errors.into_iter().next().expect("resolution error"))?
        );
    };
    let mut data = RegistryMetadata::default();
    let mut registry = None;
    let mut fetch = |request: &RegistryRequest, data: &mut RegistryMetadata, direct| {
        let client = match &mut registry {
            Some(client) => client,
            None => registry.insert(Registry::new(config, pnpm_major, project)?),
        };
        client.satisfy(request, data, direct)
    };
    let mut attempts = BTreeMap::new();
    loop {
        let errors = match solve(Some(&data)) {
            Ok(lock) => return Ok(lock),
            Err(errors) => errors,
        };
        let mut progress = false;
        let mut unresolved = None;
        for mut error in errors {
            let locksmith::LockfileError::ResolutionRequired {
                request: Some(ref request),
                ref path,
                ref mut message,
            } = error
            else {
                return Err(error.into());
            };
            let failure = attempts.entry(request.clone()).or_insert_with(|| {
                let fetched = fetch(request, &mut data, path.first().is_some_and(|s| s == "importers"));
                if let Err(reason) = fetched {
                    if purpose == Lookup::Converge
                        && let RegistryRequest::Manifest { name, version } = request.as_ref()
                    {
                        data.skip_manifest(name, version);
                        eprintln!("locksmith: keeping installed dependency pins for {name}@{version}: {reason}");
                    } else {
                        return Some(reason.to_string());
                    }
                }
                progress = true;
                None
            });
            if let Some(reason) = failure {
                message.clone_from(reason);
            }
            unresolved.get_or_insert(error);
        }
        if !progress {
            return Err(unresolved.expect("resolution error").into());
        }
    }
}

impl RegistrySettings {
    fn load(workspace: &Value, project: ProjectRc<'_>) -> Result<Self> {
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
                settings = working_rc()?;
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
        Ok(Self {
            rc,
            config,
            global_auth_routes,
            env_auth_routes,
        })
    }

    fn registry(&self, name: &str) -> Result<String> {
        let scope = name.split_once('/').map(|(scope, _)| scope);
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
        if qualified {
            return Err(format!("registry-qualified lock identity required for {name}").into());
        }
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
}
impl Registry {
    fn new(workspace: &Value, pnpm_major: Option<u64>, project: ProjectRc<'_>) -> Result<Self> {
        let mut settings = RegistrySettings::load(workspace, project)?;
        let config = &mut settings.config;
        let rc = &settings.rc;
        for (key, field) in [
            ("minimum_release_age", "minimumReleaseAge"),
            ("minimum_release_age_strict", "minimumReleaseAgeStrict"),
            ("minimum_release_age_exclude", "minimumReleaseAgeExclude"),
            (
                "minimum_release_age_ignore_missing_time",
                "minimumReleaseAgeIgnoreMissingTime",
            ),
            ("resolution_mode", "resolutionMode"),
            ("trust_policy", "trustPolicy"),
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
        let min_age = match config.get("minimumReleaseAge") {
            Some(v) => v
                .as_u64()
                .ok_or("minimumReleaseAge must be a nonnegative number of minutes")?,
            None => rc
                .get("minimum-release-age")
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(if pnpm_major.is_some_and(|v| v >= 11) {
                    1440
                } else {
                    0
                }),
        };
        let mut builder = Agent::config_builder()
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
            settings,
            packages: BTreeMap::new(),
            min_age,
            now: Timestamp::now().as_second(),
        })
    }

    fn fetch_package(&self, name: &str, base: &str) -> Result<Packument> {
        let url = format!("{base}{}", name.replace('/', "%2f"));
        let mut request = self.agent.get(&url).header(
            "Accept",
            if self.min_age > 0 {
                "application/json"
            } else {
                "application/vnd.npm.install-v1+json, application/json"
            },
        );
        if let Some(auth) = self.settings.auth(&url, name)? {
            request = request.header("Authorization", &auth);
        }
        let mut res = request
            .call()
            .map_err(|e| format!("registry metadata for {name}: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("registry metadata for {name}: HTTP {}", res.status()).into());
        }
        let doc: Packument = serde_json::from_reader(BufReader::new(res.body_mut().as_reader()))
            .map_err(|e| format!("registry metadata for {name}: {e}"))?;
        if doc.name != name {
            return Err(format!("registry returned a different package for {name}").into());
        }
        Ok(doc)
    }

    fn satisfy(
        &mut self,
        req: &RegistryRequest,
        data: &mut RegistryMetadata,
        direct: bool,
    ) -> Result<()> {
        let name = req.name();
        if self
            .settings
            .config
            .get("pnprServer")
            .is_some_and(|value| !value.is_null() && value != false)
        {
            return Err("registry metadata lookup requires the configured pnprServer".into());
        }
        let base = self.settings.registry(name)?;
        if !self.packages.contains_key(name) {
            let result = self
                .fetch_package(name, &base)
                .map_err(|error| error.to_string());
            self.packages.insert(name.to_owned(), result);
        }
        let doc = self.packages[name].as_ref().map_err(String::as_str)?;
        let selected = match req {
            RegistryRequest::Manifest { version, .. } => version.as_str(),
            RegistryRequest::Version { selector, .. } => self.select(doc, selector, direct)?,
        };
        let raw = doc
            .versions
            .get(selected)
            .ok_or_else(|| format!("registry has no release {name}@{selected}"))?;
        data.insert(
            req,
            serde_json::from_str(raw.get())?,
            &base,
            self.settings.config["lockfileIncludeTarballUrl"]
                .as_bool()
                .unwrap_or(false),
        )?;
        Ok(())
    }
}
