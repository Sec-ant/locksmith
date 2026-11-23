use std::{
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    str::FromStr,
    time::Duration,
};

use locksmith::{GenerateContext, GenerationHooks, LockfileError};
use node_semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Result, workspace};

#[cfg(feature = "pnpmfile")]
mod embedded;
mod pre;

const HOOKS: &str = include_str!("hooks/pnpmfile.cjs");
const NODE_SERVER: &str = r#"
const send = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);
const dispatch = nodeHookHost();
(async () => {
  for await (const line of require('node:readline').createInterface({ input: process.stdin })) {
    let reply;
    try { reply = { value: await dispatch(JSON.parse(line)) }; }
    catch (error) { reply = { error: String(error.stack || error) }; }
    send(JSON.stringify(reply) + '\n');
  }
})().catch(e => { console.error(e); process.exitCode = 1; });
"#;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Runtime {
    #[default]
    Node,
    Embedded,
    Off,
}

impl FromStr for Runtime {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "node" => Ok(Self::Node),
            "embedded" => Ok(Self::Embedded),
            "off" => Ok(Self::Off),
            _ => Err("expected node, embedded or off"),
        }
    }
}

/// A Node process that evaluates the pnpmfile in the workspace directory.
/// `LOCKSMITH_NODE` selects the executable; the Node API passes its own.
/// A reader thread forwards response lines so a request can time out.
struct Channel {
    replies: std::sync::mpsc::Receiver<io::Result<String>>,
    write: Option<BufWriter<ChildStdin>>,
    child: Child,
    timeout: Option<Duration>,
}

impl Channel {
    fn node(timeout: Option<Duration>) -> Result<Self> {
        let node = std::env::var_os("LOCKSMITH_NODE").unwrap_or_else(|| "node".into());
        let script = [HOOKS, NODE_SERVER].join("\n");
        let mut child = Command::new(&node)
            .args(["-e", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start {} for pnpmfile: {e}", node.display()))?;
        let stdout: ChildStdout = child.stdout.take().expect("piped stdout");
        let (send, replies) = std::sync::mpsc::channel();
        // The thread ends when the child closes stdout, including after Drop kills it.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            replies,
            write: Some(BufWriter::new(child.stdin.take().expect("piped stdin"))),
            child,
            timeout,
        })
    }
    fn call(&mut self, request: &impl Serialize) -> Result<Value> {
        let writer = self.write.as_mut().expect("open channel");
        serde_json::to_writer(&mut *writer, request)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let closed = || -> Box<dyn std::error::Error> {
            "Node hook host closed its response channel".into()
        };
        let line = match self.timeout {
            Some(limit) => self.replies.recv_timeout(limit).map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => {
                    format!("pnpmfile hook did not respond within {}s", limit.as_secs()).into()
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => closed(),
            })?,
            None => self.replies.recv().map_err(|_| closed())?,
        }?;
        let mut reply: Value = serde_json::from_str(&line)?;
        if let Some(e) = reply["error"].as_str() {
            return Err(e.to_owned().into());
        }
        reply
            .as_object_mut()
            .and_then(|r| r.remove("value"))
            .ok_or_else(|| "missing Node hook result".into())
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.write.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum Host {
    Node(Channel),
    #[cfg(feature = "pnpmfile")]
    Embedded(embedded::Engine),
}
impl Host {
    fn call(&mut self, op: &str, value: &Value) -> Result<Value> {
        #[derive(Serialize)]
        struct Request<'a> {
            op: &'a str,
            value: &'a Value,
        }
        let request = Request { op, value };
        match self {
            Self::Node(host) => host.call(&request),
            #[cfg(feature = "pnpmfile")]
            Self::Embedded(host) => host.call(&request),
        }
    }

    fn update_config(&mut self, config: &mut Value, version: Option<&Version>) -> Result<()> {
        let field = match version {
            Some(v)
                if (v.major() == 10 && v.minor() >= 8) || (v.major() == 11 && v.minor() < 23) =>
            {
                Some("registries")
            }
            Some(v) if v.major() == 11 || v >= &Version::from((12, 4, 1)) => {
                Some("registriesByScope")
            }
            None => Some("registriesByScope"),
            _ => None,
        };
        let checksum = config.get("pnpmfileChecksum").cloned();
        if let Some(field) = field {
            let routes =
                crate::registry::hook_registries(config, version.map_or(12, Version::major))?;
            let configured_registry = config.get("registry").cloned();
            let configured_registries = config.get("registries").cloned();
            config["registry"] = routes["default"].clone();
            config[field] = routes.clone();
            *config = self.call("updateConfig", config)?;
            let settings = config.as_object_mut().expect("updated configuration");
            if settings.get("registry") == routes.get("default") {
                settings.remove("registry");
                if let Some(registry) = configured_registry {
                    settings.insert("registry".into(), registry);
                }
            }
            let returned = settings.remove(field);
            if version.is_some_and(|v| v.major() < 12) {
                // pnpm 11.23+ takes changes to the routes through
                // registriesByScope. It ignores a returned registries setting.
                settings.remove("registries");
                if let Some(original) = configured_registries {
                    settings.insert("registries".into(), original);
                }
            } else if let (Some(Value::Object(mut original)), Some(returned)) = (
                configured_registries,
                settings.get("registries").and_then(Value::as_object),
            ) {
                original.extend(returned.clone());
                settings.insert("registries".into(), original.into());
            }
            // Leaving the projection unchanged preserves routing via the original
            // settings, including changes to the declarative registries setting.
            if let Some(returned) = returned.filter(|returned| returned != &routes) {
                settings.insert(
                    "registriesByScope".into(),
                    crate::registry::updated_hook_registries(returned, &routes["default"])?,
                );
            }
        } else {
            *config = self.call("updateConfig", config)?;
        }
        if let Some(checksum) = checksum {
            config["pnpmfileChecksum"] = checksum;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Operations {
    active: bool,
    pre_resolution: bool,
    read_package: bool,
    update_config: bool,
    after_all_resolved: bool,
}

pub struct Hooks {
    host: Host,
    operations: Operations,
}
impl Hooks {
    pub fn open(
        runtime: Runtime,
        config: &mut Value,
        version: Option<&Version>,
    ) -> Result<Option<Self>> {
        if config["ignorePnpmfile"] == true {
            return Ok(None);
        }
        let mut files: Vec<(String, bool)> = workspace::hook_paths(config)?
            .into_iter()
            .map(|path| (path, false))
            .collect();
        if let Some(global) = config.get("globalPnpmfile") {
            let global = global.as_str().ok_or("globalPnpmfile expects a path")?;
            files.insert(0, (global.into(), true));
        }
        // pnpm 9 and 10 run without a configured pnpmfile that does not exist.
        if version.is_some_and(|v| v.major() < 11) {
            files.retain(|(path, _)| Path::new(path).exists());
        }
        if files.is_empty() {
            return Ok(None);
        }
        let mut host = match runtime {
            Runtime::Off => {
                let names: Vec<_> = files.iter().map(|(path, _)| path.as_str()).collect();
                return Err(format!(
                    "{} requires pnpmfile evaluation; --hook-runtime off is selected",
                    names.join(", ")
                )
                .into());
            }
            // pnpm 12 gives each hook call 30 seconds.
            Runtime::Node => Host::Node(Channel::node(
                version
                    .is_none_or(|v| v.major() >= 12)
                    .then_some(Duration::from_secs(30)),
            )?),
            #[cfg(feature = "pnpmfile")]
            Runtime::Embedded => Host::Embedded(embedded::Engine::new()?),
            #[cfg(not(feature = "pnpmfile"))]
            Runtime::Embedded => {
                return Err(
                    "embedded hook runtime requires a build with --features pnpmfile".into(),
                );
            }
        };
        let paths = files
            .iter()
            .map(|(path, global)| Ok((Path::new(path).canonicalize()?, *global)))
            .collect::<io::Result<Vec<_>>>()?;
        let pre = match version.map(|v| (v.major(), v.minor())) {
            Some((9, _) | (10, 0..=9)) => "global",
            Some((10, 10..=12)) => "single",
            _ => "all",
        };
        // pnpm 10 takes updateConfig only from the project pnpmfile.
        let global_update_config = version.is_none_or(|v| v.major() >= 11);
        let operations: Operations = serde_json::from_value(host.call(
            "open",
            &json!({"files":paths,"pre":pre,"globalUpdateConfig":global_update_config,
                "releaseFields":crate::registry::RESOLUTION_FIELDS}),
        )?)?;
        if !operations.active {
            config
                .as_object_mut()
                .expect("workspace configuration")
                .remove("pnpmfileChecksum");
            return Ok(None);
        }
        // pnpm 12 records the digest of an empty project pnpmfile list when
        // only a global pnpmfile is active.
        if version.is_none_or(|v| v.major() >= 12) && config.get("pnpmfileChecksum").is_none() {
            config["pnpmfileChecksum"] = json!({});
        }
        if operations.update_config {
            host.update_config(config, version)?;
        }
        Ok(Some(Self { host, operations }))
    }
    pub fn reads_packages(&self) -> bool {
        self.operations.read_package
    }
    fn transform(
        &mut self,
        op: &str,
        id: &str,
        value: Value,
    ) -> std::result::Result<Value, LockfileError> {
        self.host
            .call(op, &value)
            .map_err(|e| LockfileError::ResolutionRequired {
                path: vec![op.into(), id.into()],
                message: e.to_string(),
                request: None,
            })
    }
}

impl GenerationHooks for Hooks {
    fn reads_packages(&self) -> bool {
        self.operations.read_package
    }

    fn pre_resolution(
        &mut self,
        previous: &Value,
        input: &GenerateContext<'_>,
    ) -> std::result::Result<Option<Value>, LockfileError> {
        if !self.operations.pre_resolution {
            return Ok(None);
        }
        let value =
            pre::context(previous, input).map_err(|e| LockfileError::ResolutionRequired {
                path: vec!["preResolution".into()],
                message: e.to_string(),
                request: None,
            })?;
        let wanted = self.transform("preResolution", "pnpm-lock.yaml", value)?;
        Ok((!wanted.is_null()).then_some(wanted))
    }
    fn read_package(
        &mut self,
        id: &str,
        value: Value,
    ) -> std::result::Result<Value, LockfileError> {
        if !self.operations.read_package {
            return Ok(value);
        }
        self.transform("readPackage", id, value)
    }
    fn read_dependency(
        &mut self,
        id: &str,
        value: Value,
    ) -> std::result::Result<Value, LockfileError> {
        if self.operations.read_package {
            self.transform("readDependency", id, value)
        } else {
            Ok(value)
        }
    }
    fn after_all_resolved(&mut self, value: Value) -> std::result::Result<Value, LockfileError> {
        if self.operations.after_all_resolved {
            self.transform("afterAllResolved", "pnpm-lock.yaml", value)
        } else {
            Ok(value)
        }
    }
}
