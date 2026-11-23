use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tempfile::tempdir;

const CLI: &str = env!("CARGO_BIN_EXE_locksmith");
const EMPTY_WORKSPACE_LOCK: &str = "lockfileVersion: '9.0'\nimporters: {'.': {}}\n";
const INTEGRITY: &str = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

struct Response {
    path: String,
    auth: Option<String>,
}

struct Registry {
    base: String,
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<Vec<Response>>,
}

impl Registry {
    fn serve(mut packages: BTreeMap<&'static str, Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        for (name, doc) in &mut packages {
            for (version, body) in doc["versions"].as_object_mut().unwrap() {
                body["name"] = (*name).into();
                body["version"] = version.clone().into();
                body["dist"] = json!({"integrity":INTEGRITY, "tarball":format!("{base}/artifacts/{name}-{version}.tgz")});
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if done.load(Ordering::Acquire) {
                    break;
                }
                let mut lines = BufReader::new(&stream).lines().map(Result::unwrap);
                let request = lines.next().unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let auth = lines.take_while(|line| !line.is_empty()).find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_owned())
                });
                let (registry, package) = path.rsplit_once('/').unwrap();
                let name = percent_encoding::percent_decode_str(package)
                    .decode_utf8()
                    .unwrap();
                requests.push(Response {
                    path: format!("{registry}/{name}"),
                    auth,
                });
                let (status, body) = packages
                    .get(name.as_ref())
                    .map_or(("404 Not Found", "{}".to_owned()), |doc| {
                        ("200 OK", doc.to_string())
                    });
                write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            requests
        });
        Self { base, stop, thread }
    }
    fn finish(self) -> Vec<Response> {
        self.stop.store(true, Ordering::Release);
        let _wake = std::net::TcpStream::connect(self.base.trim_start_matches("http://")).unwrap();
        self.thread.join().unwrap()
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    command(dir, program).args(args).output().unwrap()
}

fn command(dir: &Path, program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_PROXY", "127.0.0.1");
    command
}
fn write(dir: &Path, name: &str, doc: &Value) {
    fs::write(dir.join(name), doc.to_string()).unwrap();
}

fn write_empty_lock(root: &Path) {
    fs::write(root.join("pnpm-lock.yaml"), EMPTY_WORKSPACE_LOCK).unwrap();
}

#[test]
fn update_config_reads_effective_registry_routes_and_redirects_metadata() {
    for (version, field, body, dependency, falls_back) in [
        (
            "10.12.4",
            "registries",
            "return {...config, registries: {default: target}};",
            "@acme/lib",
            false,
        ),
        (
            "11.22.0",
            "registries",
            "return {...config, registries: {default: target}};",
            "lib",
            false,
        ),
        (
            "11.23.0",
            "registriesByScope",
            "return {...config, registriesByScope: {default: target}};",
            "@acme/lib",
            false,
        ),
        (
            "11.23.0",
            "registriesByScope",
            "return {...config, registries: {default: target}};",
            "lib",
            true,
        ),
        (
            "12.15.0",
            "registriesByScope",
            "return {...config, registries: {default: target}};",
            "lib",
            false,
        ),
        (
            "12.15.0",
            "registriesByScope",
            "return {...config, registries: {default: target}};",
            "@configured/lib",
            true,
        ),
        (
            "12.15.0",
            "registriesByScope",
            "return {...config, registriesByScope: {default: target}};",
            "@acme/lib",
            false,
        ),
        (
            "12.15.0",
            "registriesByScope",
            "return {...config, registriesByScope: {'@acme': target}};",
            "lib",
            true,
        ),
    ] {
        let project = tempdir().unwrap();
        let root = project.path();
        let manifest =
            json!({"name":dependency,"versions":{"1.0.0":{}}, "dist-tags":{"latest":"1.0.0"}});
        let old = Registry::serve(if falls_back {
            BTreeMap::from([(dependency, manifest.clone())])
        } else {
            BTreeMap::new()
        });
        let new = Registry::serve(BTreeMap::from([(dependency, manifest)]));
        write_empty_lock(root);
        write(
            root,
            "package.json",
            &json!({"dependencies":{(dependency):"1.0.0"}}),
        );
        write(
            root,
            "pnpm-workspace.yaml",
            &json!({"minimumReleaseAge":0,"registries":{"@configured":old.base}}),
        );
        fs::write(
            root.join(".npmrc"),
            format!(
                "registry={0}/\n@acme:registry={0}/\n{1}/:_authToken=previous-token\n",
                old.base,
                old.base.strip_prefix("http:").unwrap()
            ),
        )
        .unwrap();
        fs::write(
            root.join(".pnpmfile.cjs"),
            format!(
                r#"exports.hooks = {{ updateConfig(config) {{
                    const target = {target:?};
                    if (config.registry !== {original:?} ||
                        config.{field}['@acme'] !== {original:?} ||
                        config.{field}['@jsr'] !== 'https://npm.jsr.io/') {{
                        throw new Error('expected the effective registry routing');
                    }}
                    {body}
                }} }};"#,
                original = format!("{}/", old.base),
                target = format!("{}/", new.base),
            ),
        )
        .unwrap();
        let result = output(run(
            root,
            CLI,
            &[
                "generate",
                "--pnpm-version",
                version,
                "--allow-network",
                "--stdout",
            ],
        ));
        assert_eq!(
            result["importers"]["."]["dependencies"][dependency]["version"],
            "1.0.0"
        );
        let previous = old.finish();
        let redirected = new.finish();
        let (requested, idle, auth) = if falls_back {
            (&previous, &redirected, Some("Bearer previous-token"))
        } else {
            (&redirected, &previous, None)
        };
        assert!(idle.is_empty());
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].path, format!("/{dependency}"));
        assert_eq!(requested[0].auth.as_deref(), auth);
    }
}

#[test]
fn an_identity_update_config_preserves_the_configured_scope_routes() {
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    let publication =
        |name| json!({"name":name,"versions":{"1.0.0":{}}, "dist-tags":{"latest":"1.0.0"}});
    let default = Registry::serve(BTreeMap::from([("lib", publication("lib"))]));
    let scoped = Registry::serve(BTreeMap::from([("@scope/lib", publication("@scope/lib"))]));
    fs::write(root.join(".npmrc"), format!("registry={}/\n", default.base)).unwrap();
    write(
        root,
        "package.json",
        &json!({"dependencies":{"lib":"1","@scope/lib":"1"}}),
    );
    write(
        root,
        "pnpm-workspace.yaml",
        &json!({
            "minimumReleaseAge":0,
            "registriesByScope":{
                "default":format!("{}/", default.base),
                "@scope":format!("{}/", scoped.base),
            }
        }),
    );
    fs::write(
        root.join(".pnpmfile.cjs"),
        "exports.hooks = { updateConfig(config) {
            require('node:fs').writeFileSync('update-config-ran', '');
            return config;
        } };",
    )
    .unwrap();
    let result = output(run(
        root,
        CLI,
        &[
            "generate",
            "--pnpm-version=12.15.0",
            "--allow-network",
            "--stdout",
        ],
    ));
    assert!(root.join("update-config-ran").is_file());
    assert_eq!(
        result["importers"]["."]["dependencies"]["lib"]["version"],
        "1.0.0"
    );
    assert_eq!(
        result["importers"]["."]["dependencies"]["@scope/lib"]["version"],
        "1.0.0"
    );
    assert_eq!(default.finish().first().unwrap().path, "/lib");
    assert_eq!(scoped.finish().first().unwrap().path, "/@scope/lib");
}

#[test]
fn generate_requires_a_lock_before_running_hooks_or_contacting_the_registry() {
    let project = tempdir().unwrap();
    let root = project.path();
    write(
        root,
        "package.json",
        &json!({"packageManager":"pnpm@10.34.9","dependencies":{"missing":"1"}}),
    );
    fs::write(
        root.join(".pnpmfile.cjs"),
        "throw new Error('unexpected hook evaluation');",
    )
    .unwrap();
    let result = run(root, CLI, &["generate", "--allow-network"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("existing pnpm-lock.yaml"));
    assert!(!root.join("pnpm-lock.yaml").exists());
}

#[test]
fn a_pinned_git_manifest_can_be_replayed_offline_with_full_hook_inputs() {
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    let origin = root.join("origin");
    fs::create_dir_all(origin.join("package")).unwrap();
    write(
        &origin.join("package"),
        "package.json",
        &json!({
            "name":"git-dep","version":"1.0.0","custom":{"entry":"cli.js"}
        }),
    );
    for args in [
        vec!["init", "-q"],
        vec!["add", "package/package.json"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(run(&origin, "git", &args).status.success());
    }
    let sha = String::from_utf8(run(&origin, "git", &["rev-parse", "HEAD"]).stdout).unwrap();
    let path = origin.to_string_lossy().replace('\\', "/");
    let spec = format!(
        "git+file:///{}#{}&path:/package",
        path.trim_start_matches('/'),
        sha.trim()
    );
    write(
        root,
        "package.json",
        &json!({"name":"app","packageManager":"pnpm@10.34.9","dependencies":{"git-dep":spec}}),
    );
    fs::write(
        root.join(".pnpmfile.cjs"),
        r#"
module.exports = { hooks: { readPackage(pkg) {
  if (pkg.name === 'git-dep') pkg.bin = { cli: pkg.custom.entry };
  return pkg;
} } };"#,
    )
    .unwrap();
    let first = run(root, CLI, &["generate", "--stdout", "--allow-network"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let doc: Value = serde_saphyr::from_slice(&first.stdout).unwrap();
    let record = doc["packages"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(record["resolution"]["commit"], sha.trim());
    assert_eq!(record["resolution"]["path"], "/package");
    assert_eq!(record["hasBin"], true);
    fs::rename(&origin, root.join("unavailable")).unwrap();
    for host in if cfg!(feature = "pnpmfile") {
        vec!["node", "embedded"]
    } else {
        vec!["node"]
    } {
        let again = run(root, CLI, &["generate", "--stdout", "--hook-runtime", host]);
        assert!(
            again.status.success(),
            "{}",
            String::from_utf8_lossy(&again.stderr)
        );
        assert_eq!(first.stdout, again.stdout);
    }
}

#[test]
fn generate_preserves_an_unsupported_previous_lock() {
    let project = tempdir().unwrap();
    let root = project.path();
    write(
        root,
        "package.json",
        &json!({"packageManager": "pnpm@10.34.9"}),
    );
    fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '6.0'\n").unwrap();
    let result = run(root, CLI, &["generate"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(root.join("pnpm-lock.yaml")).unwrap(),
        "lockfileVersion: '6.0'\n"
    );
}

#[test]
fn generate_evaluates_local_async_hooks_with_the_selected_runtime() {
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    write(
        root,
        "package.json",
        &json!({"name":"root","packageManager":"pnpm@10.34.9","dependencies":{"choice":"workspace:one@*"}}),
    );
    write(
        root,
        "pnpm-workspace.yaml",
        &json!({"packages":["members/*"]}),
    );
    for name in ["one", "two"] {
        let dir = root.join("members").join(name);
        fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "package.json",
            &json!({"name":name,"version":"1.0.0"}),
        );
    }
    write(root, "choice.json", &json!("workspace:two@*"));
    fs::write(
        root.join("target.cjs"),
        r#"module.exports = class Target {};
const check = require('./check.cjs');
module.exports.choice = check(new module.exports());"#,
    )
    .unwrap();
    fs::write(
        root.join("check.cjs"),
        r#"const Target = require('./target.cjs');
module.exports = instance => instance instanceof Target && require('./choice.json');"#,
    )
    .unwrap();
    fs::write(
        root.join(".pnpmfile.cjs"),
        r#"const { choice } = require('./target.cjs');
const path = require('node:path');
const fs = require('node:fs/promises');
setInterval(() => {}, 60000);
module.exports = { hooks: { async readPackage(pkg) {
  if (pkg.name === 'root') {
    await new Promise(resolve => setTimeout(resolve, 1));
    const file = await fs.readFile(path.join(__dirname, 'choice.json'), 'utf8');
    if (path.posix.sep !== '/' || JSON.parse(file) !== choice) throw new Error('invalid local module context');
    if (path.normalize(path.relative('clients/app', 'shared/config')) !== ['..','..','shared','config'].join(path.sep)) throw new Error('incorrect relative workspace link');
    pkg.dependencies.choice = choice;
  }
  return pkg;
} } };"#,
    )
    .unwrap();
    let node = output(run(root, CLI, &["generate", "--stdout"]));
    assert_eq!(
        node["importers"]["."]["dependencies"]["choice"],
        json!({"specifier":"workspace:two@*","version":"link:members/two"})
    );
    assert!(node["pnpmfileChecksum"].is_string());
    let off = run(root, CLI, &["generate", "--hook-runtime", "off"]);
    assert!(!off.status.success());
    assert!(String::from_utf8_lossy(&off.stderr).contains(".pnpmfile.cjs"));
    let embedded = run(
        root,
        CLI,
        &["generate", "--hook-runtime", "embedded", "--stdout"],
    );
    if cfg!(feature = "pnpmfile") {
        assert_eq!(output(embedded), node);
    } else {
        assert!(!embedded.status.success());
        assert!(String::from_utf8_lossy(&embedded.stderr).contains("--features pnpmfile"));
    }
    assert_eq!(
        fs::read_to_string(root.join("pnpm-lock.yaml")).unwrap(),
        EMPTY_WORKSPACE_LOCK
    );
}

#[test]
fn pre_resolution_can_change_the_wanted_pin_before_the_solver_reads_it() {
    let project = tempdir().unwrap();
    let root = project.path();
    let registry = Registry::serve(BTreeMap::from([(
        "lib",
        json!({
            "name":"lib","dist-tags":{"latest":"1.1.0"},"versions":{"1.0.0":{},"1.1.0":{}}
        }),
    )]));
    write(root, "package.json", &json!({"dependencies":{"lib":"^1"}}));
    write(
        root,
        "pnpm-workspace.yaml",
        &json!({"registry":registry.base,"storeDir":"my-store","minimumReleaseAge":0,"pmOnFail":"ignore"}),
    );
    let old = format!(
        "lockfileVersion: '9.0'\nsettings: {{autoInstallPeers: true, excludeLinksFromLockfile: false}}\nimporters:\n  .:\n    dependencies:\n      lib: {{specifier: ^1, version: 1.0.0}}\npackages:\n  lib@1.0.0: {{resolution: {{integrity: {INTEGRITY}}}}}\n  lib@1.1.0: {{resolution: {{integrity: {INTEGRITY}}}}}\nsnapshots:\n  lib@1.0.0: {{}}\n  lib@1.1.0: {{}}\n"
    );
    fs::write(root.join("pnpm-lock.yaml"), &old).unwrap();
    fs::write(root.join(".pnpmfile.cjs"), r#"
exports.hooks = { async preResolution(ctx, logger) {
  if (ctx.existsCurrentLockfile || !ctx.existsNonEmptyWantedLockfile || ctx.lockfileDir !== process.cwd() ||
      !ctx.registries.default.startsWith('http://127.0.0.1:') || !ctx.storeDir.includes('my-store')) throw Error('wrong pre-resolution context');
  logger.info('checking the wanted pin');
  const deps = ctx.wantedLockfile.importers['.'].dependencies;
  if (typeof deps.lib === 'string') {
    if (ctx.currentLockfile.importers['.'].specifiers.lib !== undefined) throw Error('unexpected installed dependency');
    deps.lib = '1.1.0';
  } else {
    if (Object.keys(ctx.currentLockfile).length) throw Error('unexpected installed lock');
    deps.lib.version = '1.1.0';
  }
} };"#).unwrap();
    for host in if cfg!(feature = "pnpmfile") {
        &["node", "embedded"][..]
    } else {
        &["node"][..]
    } {
        for (v, pin) in [
            ("9.15.9", "1.0.0"),
            ("10.12.4", "1.1.0"),
            ("11.28.12", "1.1.0"),
            ("12.21.0", "1.1.0"),
        ] {
            let out = run(
                root,
                CLI,
                &[
                    "generate",
                    "--stdout",
                    "--allow-network",
                    "--hook-runtime",
                    host,
                    "--pnpm-version",
                    v,
                ],
            );
            let doc = output(out);
            assert_eq!(doc["importers"]["."]["dependencies"]["lib"]["version"], pin);
        }
    }
    registry.finish();
    assert_eq!(
        fs::read_to_string(root.join("pnpm-lock.yaml")).unwrap(),
        old
    );
    assert!(!root.join("node_modules").exists());
}

#[test]
fn generate_resolves_links_in_both_directions_for_external_workspace_members() {
    let project = tempdir().unwrap();
    let root = project.path().join("common/temp/space");
    let external = project.path().join("apps/client");
    fs::create_dir_all(&root).unwrap();
    write_empty_lock(&root);
    fs::create_dir_all(&external).unwrap();
    write(
        &root,
        "package.json",
        &json!({"name":"workspace-shell","version":"1.0.0","dependencies":{"client":"workspace:*"}}),
    );
    write(
        &root,
        "pnpm-workspace.yaml",
        &json!({"packages":["../../../apps/*"]}),
    );
    write(
        &external,
        "package.json",
        &json!({"name":"client","version":"1.0.0","dependencies":{"workspace-shell":"workspace:*"}}),
    );
    let lock = output(run(
        &root,
        CLI,
        &["generate", "--stdout", "--pnpm-version", "9.14.4"],
    ));
    assert_eq!(
        lock["importers"]["."]["dependencies"]["client"]["version"],
        "link:../../../apps/client"
    );
    assert_eq!(
        lock["importers"]["../../../apps/client"]["dependencies"]["workspace-shell"]["version"],
        "link:../../common/temp/space"
    );
}

#[test]
fn generate_records_legacy_resolutions_with_pnpm_overrides_taking_precedence() {
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    write(
        root,
        "package.json",
        &json!({
            "packageManager": "pnpm@10.8.1", "resolutions": {"a": "1", "b": "1"},
            "pnpm": {"overrides": {"b": "2"}}
        }),
    );
    let document = output(run(root, CLI, &["generate", "--stdout"]));
    assert_eq!(document["overrides"], json!({"a": "1", "b": "2"}));
}

#[test]
fn generate_reads_settings_that_the_workspace_delegates_to_other_files() {
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    write(
        root,
        "package.json",
        &json!({"packageManager": "pnpm@12.15.0"}),
    );
    let workspace = "pmOnFail: ignore\ninclude:\n  ./more.yaml:\n    - overrides\n";
    fs::write(root.join("pnpm-workspace.yaml"), workspace).unwrap();
    fs::write(root.join("more.yaml"), "overrides:\n  a: '2'\n").unwrap();
    let document = output(run(root, CLI, &["generate", "--stdout"]));
    assert_eq!(document["overrides"], json!({"a": "2"}));
    // A delegated setting has one owner.
    let both = format!("overrides:\n  a: '1'\n{workspace}");
    fs::write(root.join("pnpm-workspace.yaml"), both).unwrap();
    let rejected = run(root, CLI, &["generate", "--stdout"]);
    assert!(!rejected.status.success());
    // Settings from a config dependency would need its installed package.
    let dependency = "pmOnFail: ignore\ninclude:\n  '@acme/policy':\n    - overrides\n";
    fs::write(root.join("pnpm-workspace.yaml"), dependency).unwrap();
    let unsupported = run(root, CLI, &["generate", "--stdout"]);
    assert!(String::from_utf8_lossy(&unsupported.stderr).contains("config dependency"));
}

#[test]
fn generate_keeps_a_malformed_published_peer_as_a_peer() {
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    let lib = root.join("lib");
    fs::create_dir(&lib).unwrap();
    write(
        root,
        "package.json",
        &json!({"dependencies":{"plugin":"file:./lib"}}),
    );
    write(
        &lib,
        "package.json",
        &json!({"name":"plugin","version":"1.0.0","peerDependencies":{"host":">="}}),
    );
    let args = |version| ["generate", "--stdout", "--pnpm-version", version];
    for version in ["9.15.9", "10.34.9"] {
        let lock = output(run(root, CLI, &args(version)));
        assert_eq!(
            lock["packages"]["plugin@file:lib"]["peerDependencies"]["host"],
            ">="
        );
        assert_eq!(lock["snapshots"]["plugin@file:lib"], json!({}));
    }
    for version in ["11.28.12", "12.21.0"] {
        assert!(!run(root, CLI, &args(version)).status.success());
    }
}

#[test]
fn generate_revalidates_expired_metadata_and_observes_new_publications() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    fs::write(
        root.join(".npmrc"),
        format!("registry=http://{}/\n", listener.local_addr().unwrap()),
    )
    .unwrap();
    write(
        root,
        "package.json",
        &json!({"packageManager":"pnpm@10.34.9","dependencies":{"lib":"*"}}),
    );
    let server = thread::spawn(move || {
        for (step, version) in ["1.0.0", "1.0.0", "1.1.0"].into_iter().enumerate() {
            let (mut stream, _) = listener.accept().unwrap();
            let mut headers = BufReader::new(&stream).lines().map(Result::unwrap);
            assert!(headers.next().unwrap().starts_with("GET /lib "));
            let validator = headers.take_while(|s| !s.is_empty()).find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("if-none-match")
                    .then(|| value.trim().to_owned())
            });
            assert_eq!(validator.as_deref(), (step > 0).then_some("\"one\""));
            let (status, etag, body) = if step == 1 {
                ("304 Not Modified", "one", String::new())
            } else {
                let data = json!({"name":"lib","dist-tags":{"latest":version},"versions":{(version):{"name":"lib","version":version,"dist":{"tarball":format!("https://registry.example/lib/-/lib-{version}.tgz"),"integrity":INTEGRITY}}}});
                (
                    "200 OK",
                    if step == 0 { "one" } else { "two" },
                    data.to_string(),
                )
            };
            write!(stream, "HTTP/1.1 {status}\r\nETag: \"{etag}\"\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    for (network, version) in [
        (true, "1.0.0"),
        (true, "1.0.0"),
        (true, "1.1.0"),
        (false, "1.1.0"),
    ] {
        let mut args = vec!["generate", "--stdout"];
        if network {
            args.push("--allow-network");
        }
        let lock = output(run(root, CLI, &args));
        assert_eq!(
            lock["importers"]["."]["dependencies"]["lib"]["version"],
            version
        );
    }
    server.join().unwrap();
}

#[test]
fn generate_resolves_a_publication_whose_registry_readme_was_truncated() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let project = tempdir().unwrap();
    let root = project.path();
    write_empty_lock(root);
    let registry = format!("http://{}/", listener.local_addr().unwrap());
    fs::write(root.join(".npmrc"), format!("registry={registry}\n")).unwrap();
    write(
        root,
        "package.json",
        &json!({"packageManager":"pnpm@10.34.9","dependencies":{"lib":"1.0.0"}}),
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = BufReader::new(&stream).lines().map(Result::unwrap);
        assert!(head.next().unwrap().starts_with("GET /lib "));
        for _ in head.take_while(|line| !line.is_empty()) {}
        let mut publication = json!({
            "name":"lib","versions":{"1.0.0":{"name":"lib","version":"1.0.0","readme":"DOC",
            "dist":{"tarball":"https://registry.example/lib/-/lib-1.0.0.tgz","integrity":INTEGRITY}}}
        }).to_string().into_bytes();
        let offset = publication.windows(3).position(|s| s == b"DOC").unwrap();
        publication[offset..offset + 3].copy_from_slice(&[0xe4, 0xbd, b'.']);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            publication.len()
        )
        .unwrap();
        stream.write_all(&publication).unwrap();
    });
    let result = run(root, CLI, &["generate", "--allow-network", "--stdout"]);
    // A failed run may never connect, which would leave the server waiting.
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    server.join().unwrap();
    let lock = locksmith::Lockfile::parse(std::str::from_utf8(&result.stdout).unwrap()).unwrap();
    assert_eq!(
        lock.project().unwrap()["packages"]["lib@1.0.0"]["resolution"]["integrity"],
        INTEGRITY
    );
}
fn output(result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_saphyr::from_slice(&result.stdout).unwrap()
}

#[test]
fn registry_selection_uses_scope_auth_release_age_and_a_single_metadata_request() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let doc = json!({"name":"@org/lib","dist-tags":{"latest":"1.2.0"},"time":{"1.0.0":"2020-01-01T00:00:00Z","1.1.0":"2020-01-02T00:00:00Z","1.2.0":"2099-01-01T00:00:00Z"},"versions":{"1.0.0":{},"1.1.0":{},"1.2.0":{}}});
    let registry = Registry::serve(BTreeMap::from([("@org/lib", doc)]));
    let address = registry.base.trim_start_matches("http:");
    fs::write(
        root.join("ci-auth.ini"),
        format!("{address}/private/:@org:_authToken=${{CI_REGISTRY_TOKEN}}\n{address}/:_authToken=other\n"),
    )
    .unwrap();
    let cfg = json!({"minimumReleaseAge":1440, "registries":{format!("{}/private/", registry.base):{"scopes":["@org"]}, format!("{}/public/",registry.base):{"scopes":["@"]}}});
    let ctx = json!({"target":"ours","manifests":{".":{"dependencies":{"a":"npm:@org/lib@^1","b":"npm:@org/lib@1.0.0"}}},"patches":{},"configurations":[cfg,cfg,cfg],"configuration":cfg});
    write(root, "ctx", &ctx);
    write(
        root,
        "lock",
        &json!({"lockfileVersion":"9.0","importers":{".":{}}}),
    );
    let mut args = vec!["merge", "lock", "lock", "lock", "--context", "ctx"];
    let offline = run(root, CLI, &args);
    assert_eq!(offline.status.code(), Some(1));
    assert!(offline.stdout.is_empty());
    args.push("--allow-network");
    let graph = output(
        command(root, CLI)
            .args(args)
            .env("PNPM_CONFIG_NPMRC_AUTH_FILE", "ci-auth.ini")
            .env("CI_REGISTRY_TOKEN", "scoped")
            .output()
            .unwrap(),
    );
    assert_eq!(
        graph["importers"]["."]["dependencies"]["a"]["version"],
        "@org/lib@1.1.0"
    );
    assert_eq!(
        graph["importers"]["."]["dependencies"]["b"]["version"],
        "@org/lib@1.0.0"
    );
    assert!(!root.join("node_modules").exists());
    let requests = registry.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/private/@org/lib");
    assert_eq!(requests[0].auth.as_deref(), Some("Bearer scoped"));
}

#[test]
fn convergence_recovers_an_original_range_only_when_it_can_reuse_a_baseline_pin() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::from([(
        "parent",
        json!({"name":"parent","versions":{"2.0.0":{"dependencies":{"child":"^1"}},"3.0.0":{"dependencies":{"child":"1.1.0"}}}}),
    )]));
    fs::write(
        root.join(".npmrc"),
        format!("registry={}/local/\n", registry.base),
    )
    .unwrap();
    let graph = |parent: &str, child: &str| json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"parent":{"specifier":parent,"version":parent}}}},"packages":{format!("parent@{parent}"):{"resolution":{"integrity":INTEGRITY}},format!("child@{child}"):{"resolution":{"integrity":INTEGRITY}}},"snapshots":{format!("parent@{parent}"):{"dependencies":{"child":child}},format!("child@{child}"): {}}});
    write(
        root,
        "package.json",
        &json!({"dependencies":{"parent":"1.0.0"}}),
    );
    write(root, "pnpm-lock.yaml", &graph("1.0.0", "1.0.0"));
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "-qm",
            "baseline",
        ],
    ] {
        assert!(run(root, "git", &args).status.success());
    }
    write(
        root,
        "package.json",
        &json!({"dependencies":{"parent":"2.0.0"}}),
    );
    write(root, "pnpm-lock.yaml", &graph("2.0.0", "1.1.0"));
    let local = output(run(root, CLI, &["converge", "HEAD"]));
    assert_eq!(
        local["snapshots"]["parent@2.0.0"]["dependencies"]["child"],
        "1.1.0"
    );
    let online = output(run(root, CLI, &["converge", "HEAD", "--allow-network"]));
    assert_eq!(
        online["snapshots"]["parent@2.0.0"]["dependencies"]["child"],
        "1.0.0"
    );
    assert!(online["snapshots"].get("child@1.1.0").is_none());
    assert!(!root.join("node_modules").exists());
    for (release, missing) in [("3.0.0", false), ("4.0.0", true)] {
        write(
            root,
            "package.json",
            &json!({"dependencies":{"parent":release}}),
        );
        write(root, "pnpm-lock.yaml", &graph(release, "1.1.0"));
        let result = run(root, CLI, &["converge", "HEAD", "--allow-network"]);
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).contains("keeping installed dependency pins"),
            missing
        );
        let doc = output(result);
        assert_eq!(
            doc["snapshots"][format!("parent@{release}")]["dependencies"]["child"],
            "1.1.0"
        );
    }
    let requests = registry.finish();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request.path == "/local/parent" && request.auth.is_none())
    );
}

#[test]
fn git_driver_resolves_a_missing_release_and_keeps_git_stages_on_registry_failure() {
    let registry = Registry::serve(BTreeMap::from([(
        "left",
        json!({"name":"left","versions":{"2.0.0":{}}}),
    )]));
    for (release, clean) in [("2.0.0", true), ("3.0.0", false)] {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let p = run(root, "git", args);
            assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
            p.stdout
        };
        let lock = |left: &str, right: &str| json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"left":{"specifier":"^1","version":left},"right":{"specifier":"^1","version":right}}}},"packages":{format!("left@{left}"):{"resolution":{"integrity":INTEGRITY}},format!("right@{right}"):{"resolution":{"integrity":INTEGRITY}}},"snapshots":{format!("left@{left}"): {},format!("right@{right}"): {}}});
        git(&["init", "-qb", "main"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.org"]);
        git(&[
            "config",
            "merge.locksmith.driver",
            &format!("'{CLI}' merge --git-driver %O %A %B %P --allow-network"),
        ]);
        fs::write(
            root.join(".gitattributes"),
            "/pnpm-lock.yaml merge=locksmith\n",
        )
        .unwrap();
        fs::write(
            root.join(".npmrc"),
            format!("registry={}/repo/\n", registry.base),
        )
        .unwrap();
        write(
            root,
            "package.json",
            &json!({"dependencies":{"left":"^1","right":"^1"}}),
        );
        write(root, "pnpm-lock.yaml", &lock("1.0.0", "1.0.0"));
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        git(&["branch", "topic"]);
        write(
            root,
            "package.json",
            &json!({"dependencies":{"left":release,"right":"^1"}}),
        );
        write(root, "pnpm-lock.yaml", &lock("1.1.0", "1.0.0"));
        git(&["commit", "-qam", "main"]);
        git(&["checkout", "-q", "topic"]);
        write(root, "pnpm-lock.yaml", &lock("1.0.0", "1.1.0"));
        git(&["commit", "-qam", "topic"]);
        git(&["checkout", "-q", "main"]);
        let result = run(root, "git", &["merge", "--no-edit", "topic"]);
        assert_eq!(
            result.status.success(),
            clean,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let content = fs::read_to_string(root.join("pnpm-lock.yaml")).unwrap();
        if clean {
            let graph: Value = serde_saphyr::from_str(&content).unwrap();
            assert_eq!(
                graph["importers"]["."]["dependencies"]["left"]["version"],
                release
            );
            assert!(git(&["ls-files", "-u"]).is_empty());
        } else {
            assert!(content.contains("<<<<<<<"));
            assert_eq!(
                String::from_utf8(git(&["ls-files", "-u", "pnpm-lock.yaml"]))
                    .unwrap()
                    .lines()
                    .count(),
                3
            );
        }
    }
    let requests = registry.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.path == "/repo/left"));
}

#[test]
fn generate_uses_only_metadata_and_reuses_its_cache_in_an_untracked_workspace() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write_empty_lock(root);
    let registry = Registry::serve(BTreeMap::from([
        (
            "a",
            json!({"name":"a","dist-tags":{"latest":"1.1.0"},"versions":{"1.0.0":{"deprecated":false,"dependencies":{"leaf":"^2"}},"1.1.0":{"dependencies":{"leaf":"^2"}}}}),
        ),
        (
            "leaf",
            json!({"name":"leaf","dist-tags":{"latest":"2.0.0"},"versions":{"2.0.0":{}}}),
        ),
    ]));
    write(
        root,
        "package.json",
        &json!({"name":"root","packageManager":"pnpm@10.34.9"}),
    );
    fs::create_dir_all(root.join("apps/web")).unwrap();
    fs::create_dir_all(root.join("apps/excluded")).unwrap();
    fs::create_dir_all(root.join("apps/web/node_modules/unrelated")).unwrap();
    write(
        root,
        "apps/web/package.json",
        &json!({"dependencies":{"alias":"npm:a@^1","a":"1.0.0","leaf":""}}),
    );
    fs::write(root.join("apps/excluded/package.json"), "broken").unwrap();
    fs::write(
        root.join("apps/web/node_modules/unrelated/package.json"),
        "broken",
    )
    .unwrap();
    write(
        root,
        "pnpm-workspace.yaml",
        &json!({"packages":["apps/**","!apps/excluded"],"registry":registry.base}),
    );
    let preview = run(root, CLI, &["generate", "--allow-network", "--stdout"]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.join("pnpm-lock.yaml")).unwrap(),
        EMPTY_WORKSPACE_LOCK
    );
    let mut paths: Vec<_> = registry.finish().into_iter().map(|r| r.path).collect();
    paths.sort();
    assert_eq!(paths, ["/a", "/leaf"]);
    let result = run(root, CLI, &["generate"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
    let stored = fs::read(root.join("pnpm-lock.yaml")).unwrap();
    assert_eq!(stored, preview.stdout);
    let lock = locksmith::Lockfile::parse(std::str::from_utf8(&stored).unwrap()).unwrap();
    let graph = lock.project().unwrap();
    assert_eq!(graph["importers"].as_object().unwrap().len(), 2);
    assert_eq!(
        graph["importers"]["apps/web"]["dependencies"]["leaf"],
        json!({"specifier":"","version":"2.0.0"})
    );
    assert_eq!(
        graph["importers"]["apps/web"]["dependencies"]["alias"]["version"],
        "a@1.0.0"
    );
    assert_eq!(
        graph["snapshots"]["a@1.0.0"]["dependencies"]["leaf"],
        "2.0.0"
    );
    assert!(!root.join("node_modules").exists());
    assert!(!root.join("apps/web/node_modules/a").exists());

    write(
        root,
        "apps/web/package.json",
        &json!({"dependencies":{"missing":"1.0.0"}}),
    );
    let failed = run(root, CLI, &["generate"]);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("missing"));
    assert_eq!(fs::read(root.join("pnpm-lock.yaml")).unwrap(), stored);
}

#[test]
fn time_based_generation_keeps_only_pnpm9s_direct_publication_keys() {
    let dir = tempdir().unwrap();
    write_empty_lock(dir.path());
    let stamp = "2025-01-21T10:40:44.553Z";
    let doc = |name: &str| json!({"name":name, "dist-tags":{"latest":"1.0.0"}, "time":{"1.0.0":stamp}, "versions":{"1.0.0":{}}});
    let registry = Registry::serve(BTreeMap::from([
        ("direct", doc("direct")),
        ("@scope/core", doc("@scope/core")),
    ]));
    write(
        dir.path(),
        "package.json",
        &json!({"packageManager":"pnpm@9.14.4","dependencies":{"direct":"1.0.0","alias":"npm:@scope/core@1.0.0"}}),
    );
    fs::write(
        dir.path().join(".npmrc"),
        format!("registry={}\nresolution-mode=time-based\n", registry.base),
    )
    .unwrap();
    let result = output(run(
        dir.path(),
        CLI,
        &["generate", "--stdout", "--allow-network"],
    ));
    assert_eq!(
        result["importers"]["."]["dependencies"]["alias"]["version"],
        "@scope/core@1.0.0"
    );
    assert_eq!(result["time"], json!({"/direct@1.0.0":stamp}));
    registry.finish();
}

#[test]
fn generate_filters_new_releases_by_time_and_publication_trust() {
    let root = tempdir().unwrap();
    write_empty_lock(root.path());
    let versions = json!({
        "1.0.0":{"_npmUser":{"trustedPublisher":{}},"dist":{"attestations":{"provenance":{}}}},
        "1.1.0":{},
        "2.0.0":{}
    });
    // The registry helper supplies dist. An approver is also publish evidence
    // on its own, independent of the tarball metadata.
    let mut doc = json!({"name":"pkg","dist-tags":{"latest":"2.0.0"},"time":{"1.0.0":"2020-01-01T00:00:00Z","1.1.0":"2020-02-01T00:00:00Z","2.0.0":"2099-01-01T00:00:00Z"},"versions":versions});
    doc["versions"]["1.0.0"]["_npmUser"]["approver"] = json!({"name":"approver"});
    let registry = Registry::serve(BTreeMap::from([("pkg", doc)]));
    write(
        root.path(),
        "package.json",
        &json!({"dependencies":{"pkg":"*"}}),
    );
    write(
        root.path(),
        "pnpm-workspace.yaml",
        &json!({"packages":[],"minimumReleaseAge":60,"trustPolicy":"no-downgrade","registry":registry.base}),
    );
    let graph = output(run(
        root.path(),
        CLI,
        &["generate", "--stdout", "--allow-network"],
    ));
    assert_eq!(
        graph["importers"]["."]["dependencies"]["pkg"]["version"],
        "1.0.0"
    );
    assert_eq!(registry.finish().len(), 1);
    // The old lock pins a newer release that lost the earlier publication's
    // approval. Updating offline must apply the same policy to an incumbent.
    let downgraded = graph.to_string().replace("1.0.0", "1.1.0");
    fs::write(root.path().join("pnpm-lock.yaml"), downgraded).unwrap();
    let offline = output(run(root.path(), CLI, &["generate", "--stdout"]));
    assert_eq!(
        offline["importers"]["."]["dependencies"]["pkg"]["version"],
        "1.0.0"
    );
}
