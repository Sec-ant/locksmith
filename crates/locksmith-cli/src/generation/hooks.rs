use std::{
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    str::FromStr,
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
struct Channel {
    read: BufReader<ChildStdout>,
    write: Option<BufWriter<ChildStdin>>,
    child: Child,
}

impl Channel {
    fn node() -> Result<Self> {
        let node = std::env::var_os("LOCKSMITH_NODE").unwrap_or_else(|| "node".into());
        let script = [HOOKS, NODE_SERVER].join("\n");
        let mut child = Command::new(&node)
            .args(["-e", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start {} for pnpmfile: {e}", node.display()))?;
        Ok(Self {
            read: BufReader::new(child.stdout.take().expect("piped stdout")),
            write: Some(BufWriter::new(child.stdin.take().expect("piped stdin"))),
            child,
        })
    }
    fn call(&mut self, request: &impl Serialize) -> Result<Value> {
        let writer = self.write.as_mut().expect("open channel");
        serde_json::to_writer(&mut *writer, request)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let mut line = String::new();
        if self.read.read_line(&mut line)? == 0 {
            return Err("Node hook host closed its response channel".into());
        }
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
        let mut files = workspace::hook_paths(config)?;
        let has_global = config.get("globalPnpmfile").is_some();
        if let Some(global) = config.get("globalPnpmfile") {
            files.insert(
                0,
                global
                    .as_str()
                    .ok_or("globalPnpmfile expects a path")?
                    .into(),
            );
        }
        if files.is_empty() || config["ignorePnpmfile"] == true {
            return Ok(None);
        }
        let mut host = match runtime {
            Runtime::Off => {
                return Err(format!(
                    "{} requires pnpmfile evaluation; --hook-runtime off is selected",
                    files.join(", ")
                )
                .into());
            }
            Runtime::Node => Host::Node(Channel::node()?),
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
            .enumerate()
            .map(|(i, p)| Ok((Path::new(p).canonicalize()?, i == 0 && has_global)))
            .collect::<io::Result<Vec<_>>>()?;
        let pre = match version.map(|v| (v.major, v.minor)) {
            Some((9, _) | (10, 0..=9)) => "global",
            Some((10, 10..=12)) => "single",
            _ => "all",
        };
        let operations: Operations = serde_json::from_value(host.call(
            "open",
            &json!({"files":paths,"pre":pre,"releaseFields":crate::registry::RESOLUTION_FIELDS}),
        )?)?;
        if !operations.active {
            config
                .as_object_mut()
                .expect("workspace configuration")
                .remove("pnpmfileChecksum");
            return Ok(None);
        }
        if operations.update_config {
            let checksum = config.get("pnpmfileChecksum").cloned();
            *config = host.call("updateConfig", config)?;
            if let Some(checksum) = checksum {
                config["pnpmfileChecksum"] = checksum;
            }
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
