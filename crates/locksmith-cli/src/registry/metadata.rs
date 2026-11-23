use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Write},
    path::PathBuf,
};

use super::{Packument, Registry, generation, trust::PublicationTrust};
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ureq::Agent;

#[derive(Serialize, Deserialize)]
struct CachedManifest {
    until: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    manifest: Value,
}

pub(super) struct MetadataRequest {
    name: String,
    url: String,
    accept: &'static str,
    auth: Option<String>,
    cache: Option<PathBuf>,
    network: Option<bool>,
    now: i64,
}

#[derive(Deserialize)]
struct CacheHeaders {
    #[serde(default, alias = "_locksmithFreshUntil")]
    until: i64,
    #[serde(default, alias = "_locksmithETag")]
    etag: Option<String>,
}

enum Representation {
    Cached(Vec<u8>),
    Revalidated(Vec<u8>, Fetch),
    Published(Vec<u8>, Fetch),
}

struct Fetch {
    body: Option<Vec<u8>>,
    until: Option<i64>,
    etag: Option<String>,
}

enum PinMetadata {
    Indexed(Packument<u8>),
    Full(Packument),
}

#[derive(Serialize)]
struct CachedPackage<'a> {
    #[serde(rename = "_locksmithPins")]
    pins: Packument<u8>,
    #[serde(flatten)]
    document: &'a Packument,
}

impl<'a> CachedPackage<'a> {
    fn new(doc: &'a Packument) -> Result<Self> {
        let pins = Packument {
            fresh_until: doc.fresh_until,
            etag: doc.etag.clone(),
            name: doc.name.clone(),
            versions: doc
                .versions
                .iter()
                .map(|(v, m)| Ok((v.clone(), m.rank()?)))
                .collect::<Result<_>>()?,
            tags: Default::default(),
            time: doc.time.clone(),
            downgrades: Default::default(),
        };
        Ok(Self {
            pins,
            document: doc,
        })
    }
}

impl Registry {
    /// Each worker takes the next ready request immediately. A slow package
    /// delays only its own result. Returning by key keeps error selection stable.
    pub(super) fn read_metadata<K: Ord + Send, V: Send>(
        &self,
        requests: Vec<(K, MetadataRequest)>,
        read: impl Fn(&MetadataRequest, &K, &Agent) -> Result<V> + Sync,
    ) -> std::collections::BTreeMap<K, std::result::Result<V, String>> {
        let workers = requests.len().min(self.network_concurrency);
        let requests = std::sync::Mutex::new(requests.into_iter());
        let agent = &self.agent;
        let drain = || {
            let mut rows = Vec::new();
            loop {
                let next = requests.lock().expect("registry queue poisoned").next();
                let Some((key, request)) = next else { break };
                let result = read(&request, &key, agent).map_err(|e| e.to_string());
                rows.push((key, result));
            }
            rows
        };
        std::thread::scope(|scope| {
            let mut tasks = Vec::new();
            for _ in 1..workers {
                // If the process has reached its thread limit, the caller and
                // the workers already running can still finish the same queue.
                let Ok(task) = std::thread::Builder::new().spawn_scoped(scope, drain) else {
                    break;
                };
                tasks.push(task);
            }
            let mut rows: std::collections::BTreeMap<_, _> = drain().into_iter().collect();
            for task in tasks {
                rows.extend(task.join().expect("registry worker panicked"));
            }
            rows
        })
    }

    /// Hooks can inspect arbitrary manifest fields. Fetch that exact release
    /// separately from the compact version index used by ordinary generation.
    pub(super) fn hook_request(
        &self,
        name: &str,
        version: &str,
        base: &str,
    ) -> Result<MetadataRequest> {
        let mut request = self.metadata_request(name, base)?;
        request.url.push('/');
        request.url.push_str(version);
        request.accept = "application/json";
        request.cache =
            generation::cache_file(&request.url, request.accept, request.auth.as_deref());
        Ok(request)
    }

    pub(super) fn metadata_request(&self, name: &str, base: &str) -> Result<MetadataRequest> {
        if self
            .config
            .get("pnprServer")
            .is_some_and(|v| !v.is_null() && v != false)
        {
            return Err("registry metadata lookup requires the configured pnprServer".into());
        }
        let url = format!("{base}{}", name.replace('/', "%2f"));
        let accept = if self.min_age > 0
            || self.config["trustPolicy"] == "no-downgrade"
            || self.config["resolutionMode"] == "time-based"
        {
            "application/json"
        } else {
            "application/vnd.npm.install-v1+json, application/json"
        };
        let auth = self.auth(&url, name)?;
        let cache = self
            .generation_network
            .and_then(|_| generation::cache_file(&url, accept, auth.as_deref()));
        Ok(MetadataRequest {
            name: name.into(),
            url,
            accept,
            auth,
            cache,
            network: self.generation_network,
            now: self.now,
        })
    }

    pub(super) fn warm(&mut self, keys: Vec<(String, String)>) -> Result<()> {
        let mut requests = Vec::new();
        for (base, name) in keys {
            let key = (base, name);
            if self.packages.contains_key(&key) || self.failed.contains_key(&key) {
                continue;
            }
            let request = self.metadata_request(&key.1, &key.0)?;
            requests.push((key, request));
        }
        for (key, result) in
            self.read_metadata(requests, |request, _, agent| request.read_pins(agent))
        {
            match result {
                Ok(PinMetadata::Indexed(doc)) => {
                    self.pinned.insert(key, doc);
                }
                Ok(PinMetadata::Full(doc)) => {
                    self.packages.insert(key, doc);
                }
                Err(msg) => {
                    self.failed.insert(key, msg);
                }
            }
        }
        Ok(())
    }
}

impl MetadataRequest {
    /// Checking an existing pin needs publication dates and trust ranks. The
    /// version manifests follow this index in the same atomic cache entry.
    fn read_pins(&self, agent: &Agent) -> Result<PinMetadata> {
        const PREFIX: &[u8] = b"{\"_locksmithPins\":";
        if let Some(path) = &self.cache {
            match fs::File::open(path) {
                Ok(file) => {
                    let mut input = BufReader::new(file);
                    if input.fill_buf()?.starts_with(PREFIX) {
                        input.consume(PREFIX.len());
                        let pins = Packument::<u8>::deserialize(
                            &mut serde_json::Deserializer::from_reader(input),
                        )?;
                        if pins.name != self.name {
                            return Err(format!(
                                "metadata cache contains a different package for {}",
                                self.name
                            )
                            .into());
                        }
                        if self.network == Some(false) || pins.fresh_until > self.now {
                            return Ok(PinMetadata::Indexed(pins));
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(PinMetadata::Full(self.read(agent)?))
    }

    fn content(&self, agent: &Agent) -> Result<Representation> {
        let bytes = match &self.cache {
            Some(path) => match fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                Err(e) => return Err(format!("metadata cache: {e}").into()),
            },
            None => None,
        };
        let (bytes, validator) = match bytes {
            Some(bytes) if self.network == Some(false) => return Ok(Representation::Cached(bytes)),
            Some(bytes) => {
                // A changed publication replaces the old body without decoding all
                // of its release records just to obtain the HTTP validator.
                let headers: CacheHeaders = serde_json::from_slice(&bytes)?;
                if headers.until > self.now {
                    return Ok(Representation::Cached(bytes));
                }
                (Some(bytes), headers.etag)
            }
            None => (None, None),
        };
        let mut response = self.fetch(agent, validator.as_deref())?;
        match response.body.take() {
            Some(data) => Ok(Representation::Published(data, response)),
            None => Ok(Representation::Revalidated(
                bytes.expect("validated cached response"),
                response,
            )),
        }
    }

    pub(super) fn manifest(&self, agent: &Agent, version: &str) -> Result<Value> {
        let (manifest, policy) = match self.content(agent)? {
            Representation::Cached(data) => (
                serde_json::from_slice::<CachedManifest>(&data)?.manifest,
                None,
            ),
            Representation::Revalidated(data, response) => (
                serde_json::from_slice::<CachedManifest>(&data)?.manifest,
                Some(response),
            ),
            Representation::Published(bytes, response) => {
                let raw = if std::str::from_utf8(&bytes).is_ok() {
                    serde_json::from_slice(&bytes)?
                } else {
                    // Node decodes response bodies as UTF-8 with replacement. Check
                    // the resolver's fields before exposing the same text to hooks.
                    serde_json::from_slice::<super::publication::Publication>(&bytes)?;
                    serde_json::from_str(&String::from_utf8_lossy(&bytes))?
                };
                (raw, Some(response))
            }
        };
        if manifest["name"] != self.name || manifest["version"] != version {
            return Err(format!("registry did not return {}@{version}", self.name).into());
        }
        let Some(policy) = policy else {
            return Ok(manifest);
        };
        let record = CachedManifest {
            until: policy.until.unwrap_or(0),
            etag: policy.etag,
            manifest,
        };
        self.store(&record, policy.until.is_some())?;
        Ok(record.manifest)
    }

    fn parse(&self, bytes: &[u8]) -> Result<Packument> {
        // Compact network responses once. Cached releases already contain validated JSON.
        let doc: Packument<super::publication::Publication> = serde_json::from_slice(bytes)
            .map_err(|e| format!("registry metadata for {}: {e}", self.name))?;
        Ok(Packument {
            name: doc.name,
            tags: doc.tags,
            time: doc.time,
            versions: doc
                .versions
                .into_iter()
                .map(|(v, p)| (v, p.into_raw()))
                .collect(),
            downgrades: Default::default(),
            fresh_until: 0,
            etag: None,
        })
    }

    pub(super) fn read(&self, agent: &Agent) -> Result<Packument> {
        let (mut doc, policy) = match self.content(agent)? {
            Representation::Cached(data) => (serde_json::from_slice::<Packument>(&data)?, None),
            Representation::Revalidated(data, response) => {
                (serde_json::from_slice::<Packument>(&data)?, Some(response))
            }
            Representation::Published(data, response) => (self.parse(&data)?, Some(response)),
        };
        if doc.name != self.name {
            return Err(format!("registry returned a different package for {}", self.name).into());
        }
        if let Some(policy) = policy {
            doc.fresh_until = policy.until.unwrap_or(0);
            doc.etag = policy.etag;
            if self.cache.is_some() && policy.until.is_some() && self.accept == "application/json" {
                self.store(&CachedPackage::new(&doc)?, true)?;
            } else {
                self.store(&doc, policy.until.is_some())?;
            }
        }
        Ok(doc)
    }

    fn fetch(&self, agent: &Agent, validator: Option<&str>) -> Result<Fetch> {
        if self.network == Some(false) {
            return Err(format!(
                "missing cached metadata for {}; use --allow-network",
                self.url
            )
            .into());
        }
        let mut req = agent.get(&self.url).header("Accept", self.accept);
        if let Some(auth) = &self.auth {
            req = req.header("Authorization", auth);
        }
        if let Some(etag) = validator {
            req = req.header("If-None-Match", etag);
        }
        let mut response = req
            .call()
            .map_err(|e| format!("registry metadata for {}: {e}", self.name))?;
        let unchanged = response.status().as_u16() == 304 && validator.is_some();
        if !response.status().is_success() && !unchanged {
            return Err(format!(
                "registry metadata for {}: HTTP {}",
                self.name,
                response.status()
            )
            .into());
        }
        let headers = response.headers();
        let cache_control = headers
            .get("cache-control")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let mut max_age: i64 = 0;
        let mut no_cache = false;
        let mut no_store = false;
        for directive in cache_control.split(',').map(str::trim) {
            if directive.eq_ignore_ascii_case("no-cache") {
                no_cache = true;
            }
            if directive.eq_ignore_ascii_case("no-store") {
                no_store = true;
            }
            if let Some((key, value)) = directive.split_once('=')
                && key.eq_ignore_ascii_case("max-age")
            {
                max_age = value.trim_matches('"').parse::<i64>().unwrap_or(0).max(0);
            }
        }
        let age = headers
            .get("age")
            .and_then(|h| h.to_str().ok()?.parse::<i64>().ok())
            .unwrap_or(0)
            .max(0);
        let etag = headers
            .get("etag")
            .and_then(|h| h.to_str().ok())
            .or(validator.filter(|_| unchanged))
            .map(str::to_owned);
        let body = if unchanged {
            None
        } else {
            let mut data = Vec::new();
            response.body_mut().as_reader().read_to_end(&mut data)?;
            Some(data)
        };
        let until = if no_cache {
            0
        } else {
            self.now.saturating_add(max_age.saturating_sub(age).max(0))
        };
        Ok(Fetch {
            body,
            until: (!no_store).then_some(until),
            etag,
        })
    }

    fn store(&self, record: &impl Serialize, cacheable: bool) -> Result<()> {
        if let Some(path) = &self.cache {
            if !cacheable {
                match fs::remove_file(path) {
                    Ok(()) => (),
                    Err(e) if e.kind() == ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
                return Ok(());
            }
            store_json(path, record)?;
        }
        Ok(())
    }
}

pub(in crate::registry) fn store_json(
    path: &std::path::Path,
    record: &impl Serialize,
) -> Result<()> {
    let parent = path.parent().expect("cache directory");
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    let mut writer = BufWriter::new(&mut file);
    serde_json::to_writer(&mut writer, record)?;
    writer.flush()?;
    drop(writer);
    file.persist(path)?;
    Ok(())
}
