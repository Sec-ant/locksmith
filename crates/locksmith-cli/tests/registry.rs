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
    fn serve(packages: BTreeMap<&'static str, Value>) -> Self {
        Self::serve_with_throttle(packages, |_| false)
    }

    fn serve_with_throttle(
        mut packages: BTreeMap<&'static str, Value>,
        mut throttle: impl FnMut(&str) -> bool + Send + 'static,
    ) -> Self {
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
                let (status, body) = if throttle(&name) {
                    ("429 Too Many Requests", "{}".to_owned())
                } else {
                    packages
                        .get(name.as_ref())
                        .map_or(("404 Not Found", "{}".to_owned()), |doc| {
                            ("200 OK", doc.to_string())
                        })
                };
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
fn merge_resolves_partial_upper_bounds_with_boolean_deprecation_metadata() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::from([(
        "widget",
        json!({"name":"widget","dist-tags":{"latest":"28.0.0"},
            "versions":{"27.2.0":{"deprecated":false},"28.0.0":{}}}),
    )]));
    let config = json!({"registry":registry.base});
    write(
        root,
        "ctx",
        &json!({"target":"ours","manifests":{".":{"dependencies":{"widget":"<=27"}}},
            "configuration":config,"configurations":[config,config,config],"patches":{}}),
    );
    write(
        root,
        "lock",
        &json!({"lockfileVersion":"9.0","importers":{".":{}}}),
    );
    let graph = output(run(
        root,
        CLI,
        &[
            "merge",
            "lock",
            "lock",
            "lock",
            "--context",
            "ctx",
            "--allow-network",
        ],
    ));
    assert_eq!(
        graph["importers"]["."]["dependencies"]["widget"]["version"],
        "27.2.0"
    );
    assert!(
        graph["packages"]["widget@27.2.0"]
            .get("deprecated")
            .is_none()
    );
    assert_eq!(registry.finish().len(), 1);
}

#[test]
fn existing_catalog_entries_follow_the_active_runners_npm_range_semantics() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "lock",
        &json!({"lockfileVersion":"9.0","importers":{".":{}},
        "catalogs":{"default":{"pkg":{"specifier":"npm:1","version":"3.0.0"}}},
        "packages":{"1@3.0.0":{"resolution":{}},"pkg@1.2.0":{"resolution":{}}},
        "snapshots":{"1@3.0.0":{},"pkg@1.2.0":{}}}),
    );
    let config = json!({"catalog":{"pkg":"npm:1"},"pmOnFail":"ignore"});
    for (runner, version, pin) in [
        ("11.0.9", "3.0.0", "1@3.0.0"),
        ("12.21.0", "1.2.0", "1.2.0"),
    ] {
        write(
            root,
            "ctx",
            &json!({"target":"ours","manifests":{".":{"packageManager":format!("pnpm@{runner}"),"dependencies":{"pkg":"catalog:"}}},"configuration":config,"configurations":[config,config,config],"patches":{}}),
        );
        let graph = output(run(
            root,
            CLI,
            &["merge", "lock", "lock", "lock", "--context", "ctx"],
        ));
        assert_eq!(
            graph["catalogs"]["default"]["pkg"],
            json!({"specifier":"npm:1","version":version}),
            "{runner}"
        );
        assert_eq!(
            graph["importers"]["."]["dependencies"]["pkg"]["version"], pin,
            "{runner}"
        );
    }
}

#[test]
fn npm_ranges_select_the_package_named_by_the_runner_and_keep_the_written_specifier() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::from([
        ("pkg", json!({"name":"pkg","versions":{"1.2.0":{}}})),
        ("1", json!({"name":"1","versions":{"3.0.0":{}}})),
    ]));
    let config = json!({"registry":registry.base,"minimumReleaseAge":0,"pmOnFail":"ignore"});
    write(
        root,
        "lock",
        &json!({"lockfileVersion":"9.0","importers":{".":{}}}),
    );
    for (runner, specifier, pin) in [
        ("12.0.0", "npm:^1", "1.2.0"),
        ("11.1.0", "npm:1", "1.2.0"),
        ("11.0.9", "npm:1", "1@3.0.0"),
        ("12.0.0", "catalog:", "1.2.0"),
    ] {
        let mut config = config.clone();
        if specifier == "catalog:" {
            config["catalog"] = json!({"pkg":"npm:^1"});
        }
        write(
            root,
            "ctx",
            &json!({"target":"ours",
            "manifests":{".":{"packageManager":format!("pnpm@{runner}"),"dependencies":{"pkg":specifier}}},
            "configuration":config,"configurations":[config,config,config],"patches":{}}),
        );
        let graph = output(run(
            root,
            CLI,
            &[
                "merge",
                "lock",
                "lock",
                "lock",
                "--context",
                "ctx",
                "--allow-network",
            ],
        ));
        assert_eq!(
            graph["importers"]["."]["dependencies"]["pkg"],
            json!({"specifier":specifier,"version":pin}),
            "{runner}"
        );
        write(root, "populated", &graph);
        let offline = output(run(
            root,
            CLI,
            &[
                "merge",
                "populated",
                "populated",
                "populated",
                "--context",
                "ctx",
            ],
        ));
        assert_eq!(offline, graph, "{runner}");
    }
    let paths: Vec<_> = registry.finish().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, ["/pkg", "/pkg", "/1", "/pkg"]);
}

#[test]
fn a_missing_package_is_requested_once_for_multiple_selectors() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::new());
    let config = json!({"registry":registry.base});
    write(
        root,
        "ctx",
        &json!({"target":"ours", "manifests":{".":{"dependencies":{"missing":"1","alias":"npm:missing@2"}}},
        "configuration":config,"configurations":[config,config,config],"patches":{}}),
    );
    write(
        root,
        "lock",
        &json!({"lockfileVersion":"9.0","importers":{".":{}}}),
    );
    let out = run(
        root,
        CLI,
        &[
            "merge",
            "lock",
            "lock",
            "lock",
            "--context",
            "ctx",
            "--allow-network",
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("404"));
    assert_eq!(registry.finish().len(), 1);
}

#[test]
fn merge_retries_a_throttled_prefetch_after_the_batch() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut throttled = false;
    let registry = Registry::serve_with_throttle(
        BTreeMap::from([
            ("one", json!({"name":"one", "versions":{"1.0.0":{}}})),
            ("two", json!({"name":"two", "versions":{"1.0.0":{}}})),
        ]),
        move |name| name == "two" && !std::mem::replace(&mut throttled, true),
    );
    let config = json!({"registry": registry.base});
    write(
        root,
        "ctx",
        &json!({"target":"ours", "manifests":{".":{"dependencies":{"one":"1.0.0","two":"1.0.0"}}},
            "configuration":config,"configurations":[config,config,config],"patches":{}}),
    );
    write(
        root,
        "lock",
        &json!({"lockfileVersion": "9.0", "importers": {".": {}}}),
    );
    let result = run(
        root,
        CLI,
        &[
            "merge",
            "lock",
            "lock",
            "lock",
            "--context",
            "ctx",
            "--allow-network",
        ],
    );
    let mut paths: Vec<_> = registry.finish().into_iter().map(|r| r.path).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["/one", "/two", "/two"]);
    let graph = output(result);
    assert_eq!(
        graph["importers"]["."]["dependencies"]["one"]["version"],
        "1.0.0"
    );
    assert_eq!(
        graph["importers"]["."]["dependencies"]["two"]["version"],
        "1.0.0"
    );
}

#[test]
fn a_published_alias_can_supply_its_parents_auto_installed_peer() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::from([
        (
            "parent",
            json!({"name":"parent","versions":{"1.0.0":{
                "dependencies":{"alias":"npm:p@1.0.0"}, "peerDependencies":{"p":"^1.0.0"}
            }}}),
        ),
        (
            "p",
            json!({"name":"p","dist-tags":{"latest":"1.1.0"},"versions":{"1.0.0":{},"1.1.0":{}}}),
        ),
    ]));
    let config = json!({"registry":registry.base, "minimumReleaseAge":0,
        "trustPolicy":"no-downgrade", "trustPolicyExclude":["parent@1.0.0","p@1.0.0"]});
    let mut context = json!({"target":"ours","manifests":{".":{"dependencies":{"parent":"1.0.0"}}},
        "configuration":config,"configurations":[config,config,config],"patches":{}});
    write(root, "ctx", &context);
    write(
        root,
        "lock",
        &json!({"lockfileVersion":"9.0","importers":{".":{}}}),
    );
    let merge = || {
        run(
            root,
            CLI,
            &[
                "merge",
                "lock",
                "lock",
                "lock",
                "--context",
                "ctx",
                "--allow-network",
            ],
        )
    };
    let graph = output(merge());
    let pin = graph["importers"]["."]["dependencies"]["parent"]["version"]
        .as_str()
        .unwrap();
    let deps = &graph["snapshots"][format!("parent@{pin}")]["dependencies"];
    assert_eq!(deps["alias"], "p@1.0.0");
    assert_eq!(deps["p"], "1.0.0");
    assert!(graph["packages"].get("p@1.1.0").is_none());
    context["configuration"]["trustPolicyExclude"] = json!(["parent@1.0.0"]);
    write(root, "ctx", &context);
    let rejected = merge();
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("p@1.0.0 requires publication trust evidence")
    );
    assert_eq!(registry.finish().len(), 4);
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
fn lowest_direct_mode_separates_direct_and_transitive_attempts() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::from([
        (
            "wrapper",
            json!({"name": "wrapper", "dist-tags": {"latest": "1.0.0"}, "versions": {"1.0.0": {"dependencies": {"dep": "^1"}}}}),
        ),
        (
            "dep",
            json!({"name": "dep", "dist-tags": {"latest": "1.2.0"}, "versions": {"1.0.0": {}, "1.2.0": {}}}),
        ),
    ]));
    let config = json!({"registry": registry.base, "minimumReleaseAge": 0, "resolutionMode": "lowest-direct"});
    write(
        root,
        "ctx",
        &json!({
            "target": "ours",
            "manifests": {".": {"dependencies": {"wrapper": "1.0.0", "dep": "^1"}}},
            "configuration": config,
            "configurations": [config, config, config],
            "patches": {}
        }),
    );
    write(
        root,
        "lock",
        &json!({"lockfileVersion": "9.0", "importers": {".": {}}}),
    );
    let graph = output(run(
        root,
        CLI,
        &[
            "merge",
            "lock",
            "lock",
            "lock",
            "--context",
            "ctx",
            "--allow-network",
        ],
    ));
    assert_eq!(
        graph["importers"]["."]["dependencies"]["dep"]["version"],
        "1.0.0"
    );
    assert_eq!(
        graph["snapshots"]["wrapper@1.0.0"]["dependencies"]["dep"],
        "1.2.0"
    );
    let _ = registry.finish();
}

#[test]
fn repair_resolves_missing_catalog_entries_in_lowest_direct_mode() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::from([(
        "dep",
        json!({"name": "dep", "dist-tags": {"latest": "1.2.0"}, "versions": {"1.0.0": {}, "1.2.0": {}}}),
    )]));
    write(
        root,
        "package.json",
        &json!({"packageManager": "pnpm@10.34.9", "dependencies": {"dep": "catalog:"}}),
    );
    write(
        root,
        "pnpm-workspace.yaml",
        &json!({
            "catalog": {"dep": "^1"},
            "registry": registry.base,
            "minimumReleaseAge": 0,
            "resolutionMode": "lowest-direct"
        }),
    );
    write(
        root,
        "pnpm-lock.yaml",
        &json!({"lockfileVersion": "9.0", "importers": {".": {}}}),
    );
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
            "base",
        ],
    ] {
        assert!(run(root, "git", &args).status.success());
    }

    let result = run(root, CLI, &["repair", "HEAD", "--allow-network"]);
    assert_eq!(registry.finish().len(), 1);
    let graph = output(result);
    assert_eq!(
        graph["catalogs"]["default"]["dep"],
        json!({"specifier": "^1", "version": "1.0.0"})
    );
    assert_eq!(
        graph["importers"]["."]["dependencies"]["dep"],
        json!({"specifier": "catalog:", "version": "1.0.0"})
    );
}

#[test]
fn repair_can_use_registry_evidence_when_parent_workspace_configuration_is_invalid() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let registry = Registry::serve(BTreeMap::from([(
        "dep",
        json!({"name": "dep", "versions": {"1.0.0": {}}}),
    )]));
    let git = |args: &[&str]| {
        let result = run(root, "git", args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "user.email", "test@example.org"]);
    write(
        root,
        "package.json",
        &json!({"dependencies":{"dep":"1.0.0"}}),
    );
    let lock = json!({"lockfileVersion":"9.0","importers":{".":{}}});
    let mut parent_lock = lock.clone();
    parent_lock["snapshots"]["unused@1.0.0"] = json!({});
    write(root, "pnpm-lock.yaml", &parent_lock);
    fs::write(root.join("pnpm-workspace.yaml"), "packages: [\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "parent"]);

    write(root, "pnpm-lock.yaml", &lock);
    write(
        root,
        "pnpm-workspace.yaml",
        &json!({"registry":registry.base,"minimumReleaseAge":0}),
    );
    git(&["commit", "-qam", "repair target"]);
    let result = run(root, CLI, &["repair", "HEAD", "--allow-network"]);
    let requests = registry.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/dep");
    assert!(String::from_utf8_lossy(&result.stderr).contains("skipping unusable parent"));
    assert_eq!(
        output(result)["importers"]["."]["dependencies"]["dep"]["version"],
        "1.0.0"
    );
}

#[test]
fn project_rc_exclusion_and_missing_time_settings_apply_during_lookup() {
    for (excluded, ignore_missing_time) in [(true, false), (false, true), (false, false)] {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let registry = Registry::serve(BTreeMap::from([(
            "123",
            json!({"name": "123", "dist-tags": {"latest": "1.0.0"}, "versions": {"1.0.0": {}}}),
        )]));
        fs::write(
        root.join(".npmrc"),
        format!(
            "registry={}\nminimum-release-age=1440\nminimum-release-age-exclude={}\nminimum-release-age-ignore-missing-time={ignore_missing_time}\ntrust-policy=no-downgrade\ntrust-policy-exclude=123\n",
            registry.base, if excluded { "123" } else { "other" }
        ),
    )
    .unwrap();
        write(
            root,
            "package.json",
            &json!({"dependencies": {"123": "1.0.0"}}),
        );
        write(
            root,
            "pnpm-lock.yaml",
            &json!({"lockfileVersion": "9.0", "importers": {".": {}}}),
        );
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
                "base",
            ],
        ] {
            assert!(run(root, "git", &args).status.success());
        }
        let result = run(root, CLI, &["repair", "HEAD", "--allow-network"]);
        let requests = registry.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/123");
        if excluded || ignore_missing_time {
            let graph = output(result);
            assert_eq!(
                graph["importers"]["."]["dependencies"]["123"]["version"],
                "1.0.0"
            );
        } else {
            assert_eq!(result.status.code(), Some(1));
            assert!(
                String::from_utf8_lossy(&result.stderr)
                    .contains("missing publish time for 123@1.0.0")
            );
        }
    }
}
