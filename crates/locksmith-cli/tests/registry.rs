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
