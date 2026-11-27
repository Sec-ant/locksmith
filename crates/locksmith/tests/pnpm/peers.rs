use super::{cfg, install, pnpm_version};
use locksmith::{Lockfile, PeerRules, PeerVersionMatching, WorkspaceConfig, inspect_peers};
use serde_json::{Value, json};
use std::process::Command;
use std::{env, fs};

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn newly_required_peers_keep_each_workspace_provider_context() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let prepare = |name: &str, extended: bool, supplier: bool, right_react: &str| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut configuration = json!({
            "packages":["left","right"],
            "dedupePeers":false,
            "resolvePeersFromWorkspaceRoot":false
        });
        if extended {
            configuration["packageExtensions"] =
                json!({"is-odd@0.1.2":{"peerDependencies":{"react-dom":"18.2.0"}}});
        }
        let mut manifests =
            BTreeMap::from([(".".to_owned(), json!({"private":true,"pnpm":configuration}))]);
        if supplier {
            manifests.get_mut(".").unwrap()["dependencies"] =
                json!({"is-odd":"0.1.2","react-dom":"18.2.0","react":"18.2.0"});
        } else {
            for (id, react) in [("left", "18.2.0"), ("right", right_react)] {
                manifests.insert(id.to_owned(), json!({"name":id,"private":true,"dependencies":{"is-even":"1.0.0","react":react}}));
            }
        }
        for (id, manifest) in &manifests {
            fs::create_dir_all(dir.join(id)).unwrap();
            fs::write(dir.join(id).join("package.json"), manifest.to_string()).unwrap();
        }
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&configuration).unwrap(),
        )
        .unwrap();
        (dir, manifests, cfg(&configuration))
    };
    let (base, _, before) = prepare("base", false, false, "18.2.0");
    let (ours, _, after) = prepare("ours", true, true, "18.2.0");
    let (theirs, _, _) = prepare("theirs", false, false, "18.3.1");
    let (expected_dir, _, _) = prepare("expected", true, false, "18.3.1");
    let locks = [&base, &ours, &theirs, &expected_dir].map(|dir| {
        install(dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    });
    let (dir, manifests, _) = prepare("merged", true, false, "18.3.1");
    let output = merge(
        &locks[0],
        &locks[1],
        &locks[2],
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&before, &after, &before],
            configuration: &after,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    for id in ["left", "right"] {
        assert_eq!(
            output.project().unwrap()["importers"][id]["dependencies"]["is-even"],
            locks[3].project().unwrap()["importers"][id]["dependencies"]["is-even"]
        );
    }
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
    let installed = Command::new("node").args(["-e", r#"
        const fs = require('node:fs');
        const {createRequire} = require('node:module');
        console.log(JSON.stringify(['left', 'right'].map(id => {
            const even = createRequire(fs.realpathSync(process.argv[1] + '/' + id + '/node_modules/is-even/package.json'));
            const odd = createRequire(even.resolve('is-odd/package.json'));
            const dom = createRequire(odd.resolve('react-dom/package.json'));
            return dom('react/package.json').version;
        })));
    "#]).arg(&dir).output().unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&installed.stdout).unwrap(),
        json!(["18.2.0", "18.3.1"])
    );
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn reconstructed_opaque_peer_context_installs_the_selected_provider() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let prepare = |name: &str, react: &str, dom: bool| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = json!({"name":"peer-reconstruction-test","private":true,"dependencies":{"react":react}});
        if dom {
            manifest["dependencies"]["react-dom"] = json!("18.2.0");
        }
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(dir.join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
        (dir, manifest)
    };
    let (base_dir, _) = prepare("base", "18.2.0", true);
    let (ours_dir, _) = prepare("ours", "18.3.1", false);
    install(&base_dir, &store, false);
    install(&ours_dir, &store, false);
    let base =
        Lockfile::parse(&fs::read_to_string(base_dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    let mut opaque = base.project().unwrap().clone();
    let reference = opaque["importers"]["."]["dependencies"]["react-dom"]["version"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = opaque["snapshots"]
        .as_object_mut()
        .unwrap()
        .remove(&format!("react-dom@{reference}"))
        .unwrap();
    let reference = "18.2.0(ffffffffffffffffffffffffffffffff)";
    opaque["snapshots"][format!("react-dom@{reference}")] = body;
    opaque["importers"]["."]["dependencies"]["react-dom"]["version"] = json!(reference);
    let opaque = Lockfile::parse(&opaque.to_string()).unwrap();
    let ours =
        Lockfile::parse(&fs::read_to_string(ours_dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    let (dir, manifest) = prepare("merged", "18.3.1", true);
    let config = WorkspaceConfig::default();
    let output = merge(
        &opaque,
        &ours,
        &opaque,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &BTreeMap::from([(".".into(), manifest)]),
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    let data = output.project().unwrap();
    let selected = data["importers"]["."]["dependencies"]["react-dom"]["version"]
        .as_str()
        .unwrap();
    assert_ne!(selected, reference);
    assert_eq!(
        data["snapshots"][format!("react-dom@{selected}")]["dependencies"]["react"],
        "18.3.1"
    );
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
    let dom = fs::canonicalize(dir.join("node_modules/react-dom")).unwrap();
    let installed: Value = serde_json::from_str(
        &fs::read_to_string(dom.parent().unwrap().join("react/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(installed["version"], "18.3.1");
}

fn check_independent_peer_upgrades(max_suffix: Option<usize>) {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let write = |name: &str, number: &str, buffer: &str| {
        let dir = root.path().join(name);
        fs::create_dir_all(dir.join("app")).unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("package.json"), "{\"private\":true}").unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            "packages:\n  - app\n  - lib\n",
        )
        .unwrap();
        if let Some(limit) = max_suffix {
            fs::write(
                dir.join("pnpm-workspace.yaml"),
                format!("packages:\n  - app\n  - lib\npeersSuffixMaxLength: {limit}\n"),
            )
            .unwrap();
        }
        fs::write(
            dir.join(".npmrc"),
            format!(
                "dedupe-injected-deps=false\n{}",
                max_suffix.map_or(String::new(), |limit| format!(
                    "peers-suffix-max-length={limit}\n"
                ))
            ),
        )
        .unwrap();
        fs::write(dir.join("app/package.json"),json!({"name":"app","private":true,"dependencies":{"lib":"workspace:*","is-number":number,"is-buffer":buffer},"dependenciesMeta":{"lib":{"injected":true}}}).to_string()).unwrap();
        fs::write(dir.join("lib/package.json"),json!({"name":"lib","version":"1.0.0","peerDependencies":{"is-number":"*","is-buffer":"*"}}).to_string()).unwrap();
        dir
    };
    let generate = |name: &str, number: &str, buffer: &str| {
        let dir = write(name, number, buffer);
        install(&dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    };
    let base = generate("base", "6.0.0", "1.1.6");
    let ours = generate("ours", "7.0.0", "1.1.6");
    let theirs = generate("theirs", "6.0.0", "2.0.5");
    let output = locksmith::merge(&base, &ours, &theirs, None).unwrap();
    output.validate().unwrap();
    let result = write("result", "7.0.0", "2.0.5");
    fs::write(result.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&result, &store, true);
    let resolved=Command::new("node").args(["-e",r#"
        const fs=require('node:fs'); const {createRequire}=require('node:module');
        const req=createRequire(fs.realpathSync(process.argv[1]+'/app/node_modules/lib/package.json'));
        console.log(JSON.stringify([req('is-number/package.json').version,req('is-buffer/package.json').version]));
    "#]).arg(&result).output().unwrap();
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&resolved.stdout).unwrap(),
        json!(["7.0.0", "2.0.5"])
    );
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn independent_peer_upgrades_merge_into_a_new_installed_instance() {
    check_independent_peer_upgrades(None);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn hashed_peer_upgrades_merge_into_a_new_installed_instance() {
    check_independent_peer_upgrades(Some(10));
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn package_upgrade_and_peer_upgrade_preserve_the_new_release_dependencies() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let write = |name: &str, dom: &str, react: &str| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("package.json"),json!({"name":"react-peer-merge","private":true,"dependencies":{"react-dom":dom,"react":react}}).to_string()).unwrap();
        dir
    };
    let generate = |name: &str, dom: &str, react: &str| {
        let dir = write(name, dom, react);
        install(&dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    };
    let base = generate("base", "18.2.0", "18.2.0");
    let ours = generate("ours", "18.3.1", "18.2.0");
    let theirs = generate("theirs", "18.2.0", "18.3.1");
    let output = locksmith::merge(&base, &ours, &theirs, None).unwrap();
    output.validate().unwrap();
    let dir = write("merged", "18.3.1", "18.3.1");
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
    let p=Command::new("node").args(["-e",r#"
        const fs=require('node:fs');const {createRequire}=require('node:module');
        const req=createRequire(fs.realpathSync(process.argv[1]+'/node_modules/react-dom/package.json'));
        console.log(JSON.stringify([req('./package.json').version,req('react/package.json').version,req('scheduler/package.json').version]));
    "#]).arg(dir).output().unwrap();
    assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
    assert_eq!(
        serde_json::from_slice::<Value>(&p.stdout).unwrap(),
        json!(["18.3.1", "18.3.1", "0.23.2"])
    );
}

fn check_contextual_alias_peers(max_suffix: Option<usize>, duplicate_aliases: bool) {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let mut lib =
        json!({"name":"lib","version":"1.0.0","peerDependencies":{"is-odd":"*","is-buffer":"*"}});
    if duplicate_aliases {
        lib["peerDependencies"]["is-number"] = json!("*");
    }
    let config = if let Some(limit) = max_suffix {
        json!({"packages":["app","lib"],"peersSuffixMaxLength":limit})
    } else {
        json!({"packages":["app","lib"]})
    };
    let write = |name: &str, number: &str, buffer: &str| {
        let dir = root.path().join(name);
        fs::create_dir_all(dir.join("app")).unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        let mut app = json!({"name":"app","private":true,"dependencies":{"lib":"workspace:*","is-odd":format!("npm:is-number@{number}"),"is-buffer":buffer},"dependenciesMeta":{"lib":{"injected":true}}});
        if duplicate_aliases {
            app["dependencies"]["is-number"] = json!(if matches!(name, "ours" | "result") {
                "7.0.0"
            } else {
                "5.0.0"
            });
        }
        fs::write(dir.join("package.json"), "{\"private\":true}").unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&config).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join(".npmrc"),
            format!(
                "dedupe-injected-deps=false\n{}",
                max_suffix.map_or(String::new(), |n| format!("peers-suffix-max-length={n}\n"))
            ),
        )
        .unwrap();
        fs::write(dir.join("app/package.json"), app.to_string()).unwrap();
        fs::write(dir.join("lib/package.json"), lib.to_string()).unwrap();
        (dir, app)
    };
    let generate = |name, number, buffer| {
        let (dir, _) = write(name, number, buffer);
        install(&dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    };
    let base = generate("base", "5.0.0", "1.1.6");
    let ours = generate("ours", "6.0.0", "2.0.5");
    let theirs = generate("theirs", "7.0.0", "1.1.6");
    let number = if max_suffix.is_some() { "6.0.0" } else { "*" };
    let (result, app) = write("result", number, "2.0.5");
    let manifests = BTreeMap::from([
        (".".into(), json!({"private":true})),
        ("app".into(), app),
        ("lib".into(), lib.clone()),
    ]);
    let patches = BTreeMap::new();
    let output = merge(
        &base,
        &ours,
        &theirs,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&config); 3],
            configuration: &cfg(&config),
            patches: &patches,
            registry: None,
        }),
    )
    .unwrap();
    let rendered = output.to_yaml().unwrap();
    fs::write(result.join("pnpm-lock.yaml"), &rendered).unwrap();
    install(&result, &store, true);
    assert_eq!(
        fs::read_to_string(result.join("pnpm-lock.yaml")).unwrap(),
        rendered
    );
    let resolved=Command::new("node").args(["-e",r#"
        const fs=require('node:fs'); const {createRequire}=require('node:module');
        const req=createRequire(fs.realpathSync(process.argv[1]+'/app/node_modules/lib/package.json'));
        const alias=req('is-odd/package.json');
        const result=[alias.name,alias.version,req('is-buffer/package.json').version];
        if(process.argv[2]==='true') result.push(req('is-number/package.json').version);
        console.log(JSON.stringify(result));
    "#]).arg(&result).arg(duplicate_aliases.to_string()).output().unwrap();
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let mut expected = json!([
        "is-number",
        if max_suffix.is_some() {
            "6.0.0"
        } else {
            "7.0.0"
        },
        "2.0.5"
    ]);
    if duplicate_aliases {
        expected.as_array_mut().unwrap().push(json!("7.0.0"));
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&resolved.stdout).unwrap(),
        expected
    );
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn contextual_merge_installs_competing_alias_peers_from_final_manifest() {
    check_contextual_alias_peers(None, false);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn contextual_merge_installs_hashed_alias_peers_from_final_manifest() {
    check_contextual_alias_peers(Some(10), false);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn aliases_of_the_same_peer_install_their_independently_selected_versions() {
    check_contextual_alias_peers(None, true);
    check_contextual_alias_peers(Some(10), true);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn contextual_peer_prereleases_follow_the_package_manager_policy() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let version = Command::new(env::var_os("PNPM_BIN").unwrap())
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout).unwrap().trim().to_owned();
    let legacy = version.split('.').next().unwrap().parse::<u32>().unwrap() < 12;
    let root_manifest = json!({"private":true,"packageManager":format!("pnpm@{version}")});
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("workspace");
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::create_dir_all(dir.join("lib")).unwrap();
    let app = json!({"name":"app","private":true,"dependencies":{"lib":"workspace:*","react":"19.0.0-rc.1"},"dependenciesMeta":{"lib":{"injected":true}}});
    let lib = json!({"name":"lib","version":"1.0.0","peerDependencies":{"react":"^19"}});
    let before = json!({"packages":["app","lib"],"strictPeerDependencies":false});
    let config = json!({"packages":["app","lib"],"strictPeerDependencies":true});
    fs::write(dir.join("package.json"), root_manifest.to_string()).unwrap();
    fs::write(
        dir.join("pnpm-workspace.yaml"),
        serde_saphyr::to_string(&before).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join(".npmrc"),
        "strict-peer-dependencies=false\ndedupe-injected-deps=false\n",
    )
    .unwrap();
    fs::write(dir.join("app/package.json"), app.to_string()).unwrap();
    fs::write(dir.join("lib/package.json"), lib.to_string()).unwrap();
    let store = root.path().join("store");
    install(&dir, &store, false);
    let input = Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    let manifests = BTreeMap::from([
        (".".into(), root_manifest),
        ("app".into(), app),
        ("lib".into(), lib),
    ]);
    let patches = BTreeMap::new();
    let output = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before); 3],
            configuration: &cfg(&config),
            patches: &patches,
            registry: None,
        }),
    );
    fs::write(
        dir.join("pnpm-workspace.yaml"),
        serde_saphyr::to_string(&config).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join(".npmrc"),
        "strict-peer-dependencies=true\ndedupe-injected-deps=false\n",
    )
    .unwrap();
    if !legacy {
        let output = output.unwrap();
        assert!(!inspect_peers(&output, &PeerRules::default(), None).is_empty());
        // A fresh install exercises peer resolution. Reusing a lock can skip
        // peer checks even when strict mode has changed.
        let strict_dir = root.path().join("strict");
        for (id, manifest) in &manifests {
            let folder = if id == "." {
                strict_dir.clone()
            } else {
                strict_dir.join(id)
            };
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("package.json"), manifest.to_string()).unwrap();
        }
        fs::write(
            strict_dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&config).unwrap(),
        )
        .unwrap();
        fs::write(
            strict_dir.join(".npmrc"),
            "strict-peer-dependencies=true\ndedupe-injected-deps=false\n",
        )
        .unwrap();
        let rejected = Command::new(env::var_os("PNPM_BIN").unwrap())
            .current_dir(&strict_dir)
            .env("CI", "true")
            .args([
                "install",
                "--no-frozen-lockfile",
                "--ignore-scripts",
                "--registry=https://registry.npmjs.org",
                "--config.manage-package-manager-versions=false",
                "--store-dir",
            ])
            .arg(&store)
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&rejected.stdout),
            String::from_utf8_lossy(&rejected.stderr)
        );
        assert!(log.contains("ERR_PNPM_PEER_DEP_ISSUES"), "{log}");
        return;
    }
    let output = output.unwrap();
    let rules = PeerRules {
        version_matching: PeerVersionMatching::Pnpm10,
        ..PeerRules::default()
    };
    assert!(inspect_peers(&output, &rules, None).is_empty());
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
    let resolved=Command::new("node").args(["-e",r#"
        const fs=require('node:fs');const {createRequire}=require('node:module');
        const req=createRequire(fs.realpathSync(process.argv[1]+'/app/node_modules/lib/package.json'));
        console.log(req('react/package.json').version);
    "#]).arg(&dir).output().unwrap();
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    assert_eq!(
        String::from_utf8(resolved.stdout).unwrap().trim(),
        "19.0.0-rc.1"
    );
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn deduplicated_descendant_peers_keep_the_ancestor_identity() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    for optional in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let configuration = json!({
            "packages":["app","other","wrapper","child"],
            "autoInstallPeers":false,"dedupePeerDependents":true,"dedupePeers":true,
            "peersSuffixMaxLength":10
        });
        let prepare = |name: &str, a: &str, b: &str| {
            let dir = root.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".npmrc"), "auto-install-peers=false\ndedupe-peer-dependents=true\ndedupe-peers=true\npeers-suffix-max-length=10\n").unwrap();
            let manifests = BTreeMap::from([
                (".".to_owned(), json!({"private":true,"pnpm":configuration})),
                (
                    "app".to_owned(),
                    json!({
                        "name":"app","private":true,
                        "dependencies":{"wrapper":"workspace:*","a":format!("npm:is-buffer@{a}"),"b":format!("npm:is-number@{b}")},
                        "dependenciesMeta":{"wrapper":{"injected":true}}
                    }),
                ),
                (
                    "other".to_owned(),
                    json!({
                        "name":"other","private":true,
                        "dependencies":{"wrapper":"workspace:*","a":"npm:is-buffer@2.0.5","b":"npm:is-number@7.0.0","x":"npm:is-odd@3.0.1"},
                        "dependenciesMeta":{"wrapper":{"injected":true}}
                    }),
                ),
                (
                    "wrapper".to_owned(),
                    json!({
                        "name":"wrapper","version":"1.0.0","dependencies":{"child":"workspace:*"},
                        "dependenciesMeta":{"child":{"injected":true}},"peerDependencies":{"a":"*","b":"*"}
                    }),
                ),
                (
                    "child".to_owned(),
                    json!({
                        "name":"child","version":"1.0.0","peerDependencies":{"x":"*"},
                        "peerDependenciesMeta":{"x":{"optional":optional}}
                    }),
                ),
            ]);
            for (id, manifest) in &manifests {
                fs::create_dir_all(dir.join(id)).unwrap();
                fs::write(dir.join(id).join("package.json"), manifest.to_string()).unwrap();
            }
            fs::write(
                dir.join("pnpm-workspace.yaml"),
                serde_saphyr::to_string(&configuration).unwrap(),
            )
            .unwrap();
            (dir, manifests)
        };
        let mut locks = Vec::new();
        for (name, a, b) in [
            ("base", "1.1.6", "4.0.0"),
            ("ours", "2.0.5", "4.0.0"),
            ("theirs", "1.1.6", "5.0.0"),
        ] {
            let (dir, _) = prepare(name, a, b);
            install(&dir, &store, false);
            locks.push(
                Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap(),
            );
        }
        let (dir, manifests) = prepare("result", "2.0.5", "5.0.0");
        let (expected_dir, _) = prepare("expected", "2.0.5", "5.0.0");
        install(&expected_dir, &store, false);
        let expected =
            Lockfile::parse(&fs::read_to_string(expected_dir.join("pnpm-lock.yaml")).unwrap())
                .unwrap();
        let configuration = cfg(&configuration);
        let result = merge(
            &locks[0],
            &locks[1],
            &locks[2],
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&configuration; 3],
                configuration: &configuration,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        )
        .unwrap();
        assert_eq!(
            result.project().unwrap()["importers"]["app"]["dependencies"]["wrapper"],
            expected.project().unwrap()["importers"]["app"]["dependencies"]["wrapper"]
        );
        fs::write(dir.join("pnpm-lock.yaml"), result.to_yaml().unwrap()).unwrap();
        install(&dir, &store, true);
        let p = Command::new("node").args(["-e", r#"
            const fs = require('node:fs');
            const {createRequire} = require('node:module');
            const wrapper = createRequire(fs.realpathSync(process.argv[1] + '/app/node_modules/wrapper/package.json'));
            const child = createRequire(wrapper.resolve('child/package.json'));
            console.log(JSON.stringify([wrapper('a/package.json').version, wrapper('b/package.json').version, child('x/package.json').version]));
        "#]).arg(&dir).output().unwrap();
        assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        assert_eq!(
            serde_json::from_slice::<Value>(&p.stdout).unwrap(),
            json!(["2.0.5", "5.0.0", "3.0.1"])
        );
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn linked_workspace_peers_install_after_a_provider_move() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let paths = [
        "providers/@scope/new".to_owned(),
        "providers/nested++name".to_owned(),
        format!("providers/{}{}", "x".repeat(50), "😀".repeat(45)),
    ];
    for (max_suffix, new_path) in [None, Some(10)]
        .into_iter()
        .flat_map(|limit| paths.iter().map(move |path| (limit, path)))
    {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let mut config = json!({"packages":["app","lib","providers/**"]});
        if let Some(limit) = max_suffix {
            config["peersSuffixMaxLength"] = json!(limit);
        }
        let prepare = |name: &str, provider_path: &str, buffer: &str| {
            let dir = root.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            let manifests = BTreeMap::from([
                (
                    ".".to_owned(),
                    json!({"private":true,"dependencies":{"provider":"workspace:*"},"pnpm":config}),
                ),
                (
                    "app".to_owned(),
                    json!({"name":"app","private":true,"dependencies":{"lib":"workspace:*","provider":"workspace:*","is-buffer":buffer},"dependenciesMeta":{"lib":{"injected":true}}}),
                ),
                (
                    "lib".to_owned(),
                    json!({"name":"lib","version":"1.0.0","peerDependencies":{"provider":"*","is-buffer":"*"}}),
                ),
                (
                    provider_path.to_owned(),
                    json!({"name":"provider","version":"1.0.0","main":"index.js"}),
                ),
            ]);
            for (id, manifest) in &manifests {
                fs::create_dir_all(dir.join(id)).unwrap();
                fs::write(dir.join(id).join("package.json"), manifest.to_string()).unwrap();
            }
            fs::write(
                dir.join(provider_path).join("index.js"),
                format!("module.exports = {};", json!(provider_path)),
            )
            .unwrap();
            fs::write(
                dir.join("pnpm-workspace.yaml"),
                serde_saphyr::to_string(&config).unwrap(),
            )
            .unwrap();
            if pnpm_version() < &node_semver::Version::from((10, 6, 0))
                && let Some(limit) = max_suffix
            {
                fs::write(
                    dir.join(".npmrc"),
                    format!("peers-suffix-max-length={limit}\n"),
                )
                .unwrap();
            }
            (dir, manifests)
        };
        let mut locks = Vec::new();
        for (name, path, buffer) in [
            ("base", "providers/old", "1.1.6"),
            ("ours", "providers/old", "2.0.5"),
            ("theirs", new_path.as_str(), "1.1.6"),
        ] {
            let (dir, _) = prepare(name, path, buffer);
            install(&dir, &store, false);
            locks.push(
                Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap(),
            );
        }
        let (dir, manifests) = prepare("result", new_path, "2.0.5");
        let (expected_dir, _) = prepare("expected", new_path, "2.0.5");
        install(&expected_dir, &store, false);
        let expected =
            Lockfile::parse(&fs::read_to_string(expected_dir.join("pnpm-lock.yaml")).unwrap())
                .unwrap();
        let config = cfg(&config);
        let output = merge(
            &locks[0],
            &locks[1],
            &locks[2],
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        )
        .unwrap();
        assert_eq!(
            output.project().unwrap()["importers"]["app"]["dependencies"]["lib"]["version"],
            expected.project().unwrap()["importers"]["app"]["dependencies"]["lib"]["version"]
        );
        assert!(inspect_peers(&output, &PeerRules::default(), Some(&manifests)).is_empty());
        fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
        install(&dir, &store, true);
        let p = Command::new("node").args(["-e", r#"
            const fs = require('node:fs');
            const {createRequire} = require('node:module');
            const req = createRequire(fs.realpathSync(process.argv[1] + '/app/node_modules/lib/package.json'));
            console.log(JSON.stringify([req('provider'), req('is-buffer/package.json').version]));
        "#]).arg(&dir).output().unwrap();
        assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        assert_eq!(
            serde_json::from_slice::<Value>(&p.stdout).unwrap(),
            json!([new_path, "2.0.5"])
        );
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn newly_introduced_optional_peers_follow_workspace_sharing_settings() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    for (dedupe, scoped) in [(true, false), (false, false), (true, true)] {
        if scoped && pnpm_version() < &node_semver::Version::from((12, 19, 0)) {
            continue; // projectScopedOptionalPeers was added in pnpm 12.19.
        }
        let prepare = |name: &str, zustand: &str, wrapper: bool| {
            let dir = root.path().join(format!("{dedupe}-{scoped}-{name}"));
            fs::create_dir_all(dir.join("packages/types")).unwrap();
            fs::write(
                dir.join(".npmrc"),
                format!("dedupe-injected-deps=false\ndedupe-peer-dependents={dedupe}\n"),
            )
            .unwrap();
            let config = json!({"packages":["packages/*"],"dedupeInjectedDeps":false,
                "dedupePeerDependents":dedupe,"projectScopedOptionalPeers":scoped,"overrides":{"zustand":zustand}});
            let mut manifest = json!({"private":true,"dependencies":{"react":"18.2.0","immer":"10.1.1"},"pnpm":config});
            let types = json!({"name":"locksmith-types","version":"1.0.0","dependencies":{"@types/react":"18.0.26"}});
            fs::write(dir.join("packages/types/package.json"), types.to_string()).unwrap();
            let mut manifests = BTreeMap::from([("packages/types".into(), types)]);
            if wrapper {
                manifest["dependencies"]["locksmith-wrapper"] = json!("workspace:*");
                manifest["dependenciesMeta"] = json!({"locksmith-wrapper":{"injected":true}});
                let package = json!({"name":"locksmith-wrapper","version":"1.0.0","dependencies":{"zustand":"4.3.8"}});
                fs::create_dir_all(dir.join("packages/wrapper")).unwrap();
                fs::write(
                    dir.join("packages/wrapper/package.json"),
                    package.to_string(),
                )
                .unwrap();
                manifests.insert("packages/wrapper".into(), package);
            } else {
                manifest["dependencies"]["zustand"] = json!(zustand);
                manifest["dependencies"]["@types/react"] = json!("18.0.26");
            }
            fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
            fs::write(
                dir.join("pnpm-workspace.yaml"),
                serde_saphyr::to_string(&config).unwrap(),
            )
            .unwrap();
            manifests.insert(".".into(), manifest);
            (dir, manifests, cfg(&config))
        };
        let (base, _, before) = prepare("base", "4.3.8", true);
        let (ours, _, after) = prepare("ours", "4.5.7", false);
        let (theirs, _, _) = prepare("theirs", "4.3.8", true);
        let (expected, _, _) = prepare("expected", "4.5.7", true);
        let locks = [&base, &ours, &theirs, &expected].map(|dir| {
            install(dir, &store, false);
            Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
        });
        let (dir, manifests, _) = prepare("merged", "4.5.7", true);
        let patches = BTreeMap::new();
        let output = merge(
            &locks[0],
            &locks[1],
            &locks[2],
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&before, &after, &before],
                configuration: &after,
                patches: &patches,
                registry: None,
            }),
        )
        .unwrap();
        let actual = output.project().unwrap();
        let expected = locks[3].project().unwrap();
        assert_eq!(
            actual["settings"], expected["settings"],
            "dedupe={dedupe}, scoped={scoped}"
        );
        for id in [".", "packages/wrapper"] {
            assert_eq!(
                actual["importers"][id], expected["importers"][id],
                "{id}, dedupe={dedupe}, scoped={scoped}"
            );
        }
        for (key, body) in expected["snapshots"].as_object().unwrap() {
            if key.starts_with("zustand@") || key.contains("locksmith-wrapper") {
                assert_eq!(
                    actual["snapshots"][key], *body,
                    "{key}, dedupe={dedupe}, scoped={scoped}"
                );
            }
        }
        fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
        install(&dir, &store, true);
    }
}

#[test]
#[ignore = "requires PNPM_BIN"]
fn inspection_agrees_with_fresh_strict_installs_of_local_tarballs() {
    use locksmith::{Disposition, inspect};

    let sandbox = tempfile::tempdir().unwrap();
    let store = sandbox.path().join("store");
    let source = sandbox.path().join("package");
    fs::create_dir(&source).unwrap();
    let pnpm = env::var_os("PNPM_BIN").unwrap();
    let rules = PeerRules::from_pnpm(
        &json!({}),
        PeerVersionMatching::for_pnpm(Some(pnpm_version())),
    )
    .unwrap();
    for (i, (range, version)) in [
        (">=0.11 <=15", "15.8.0"),
        (">=0.26 <=0.34", "0.34.33"),
        ("19.0.0-rc.7", "19.0.0-rc.4"),
        ("^1", "1.4.0-rc.1"),
        ("^1", "2.0.0"),
        (">1.0.0-rc.1", "1.0.0-rc.2"),
        ("<1", "1.0.0-rc.1"),
        ("<1.0.0", "1.0.0-rc.1"),
        ("^1 || workspace:^2", "2.0.0"),
        (">=2.0.0 <3.0.0", "2.1.0"),
        ("latest", "2.0.0"),
    ]
    .into_iter()
    .enumerate()
    {
        let project = sandbox.path().join(format!("project-{i}"));
        let strict = sandbox.path().join(format!("strict-{i}"));
        fs::create_dir(&project).unwrap();
        fs::create_dir(&strict).unwrap();
        for (file, manifest) in [
            ("host", json!({"name":"host","version":version})),
            (
                "plugin",
                json!({"name":"plugin","version":"1.0.0","peerDependencies":{"host":range}}),
            ),
        ] {
            fs::write(source.join("package.json"), manifest.to_string()).unwrap();
            super::assert_success(
                Command::new("tar")
                    .arg("-czf")
                    .arg(project.join(format!("{file}.tgz")))
                    .arg("-C")
                    .arg(sandbox.path())
                    .arg("package")
                    .output()
                    .unwrap(),
            );
        }
        fs::write(project.join("package.json"), json!({"private":true,"packageManager":format!("pnpm@{}",pnpm_version()),"dependencies":{"host":"file:host.tgz","plugin":"file:plugin.tgz"}}).to_string()).unwrap();
        fs::write(
            project.join("pnpm-workspace.yaml"),
            "packages: []\nautoInstallPeers: false\nminimumReleaseAge: 0\n",
        )
        .unwrap();
        fs::write(
            project.join(".npmrc"),
            "auto-install-peers=false\nmanage-package-manager-versions=false\n",
        )
        .unwrap();
        super::copy_project(&project, &strict, &store);
        let resolve = |dir: &std::path::Path, policy: &str| {
            Command::new(&pnpm)
                .current_dir(dir)
                .env("CI", "true")
                .args([
                    "install",
                    "--lockfile-only",
                    "--no-frozen-lockfile",
                    "--offline",
                    "--ignore-scripts",
                    "--reporter=ndjson",
                    policy,
                    "--store-dir",
                ])
                .arg(&store)
                .output()
                .unwrap()
        };
        super::assert_success(resolve(&project, "--strict-peer-dependencies=false"));
        let lock = Lockfile::try_from(fs::read_to_string(project.join("pnpm-lock.yaml")).unwrap())
            .unwrap();
        let diagnostic = inspect(&lock, &rules, None).unwrap();
        assert!(
            diagnostic
                .peers
                .iter()
                .all(|p| p.requirement.disposition != Disposition::Unknown),
            "unknown verdict for local tarballs: {range} with {version}"
        );
        let rejected = diagnostic
            .peers
            .iter()
            .any(|p| p.requirement.disposition == Disposition::Rejected);
        let oracle = resolve(&strict, "--strict-peer-dependencies");
        if !oracle.status.success() {
            let logs = format!(
                "{}{}",
                String::from_utf8_lossy(&oracle.stdout),
                String::from_utf8_lossy(&oracle.stderr)
            );
            assert!(logs.contains("ERR_PNPM_PEER_DEP_ISSUES"), "{logs}");
        }
        assert_eq!(
            rejected,
            !oracle.status.success(),
            "pnpm {}: {range} with {version}",
            pnpm_version()
        );
    }
}
