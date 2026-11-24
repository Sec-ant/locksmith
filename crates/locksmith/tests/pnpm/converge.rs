use super::{OURS, THEIRS, install, override_tie_break};
use locksmith::Lockfile;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn manifest_driven_convergence_installs_and_removes_unrelated_workspace_drift() {
    use locksmith::{ManifestChange, converge};
    let mut baseline = Lockfile::parse(include_str!("../fixtures/pnpm10/base.yaml"))
        .unwrap()
        .project()
        .unwrap()
        .clone();
    let other = baseline["importers"]["."].clone();
    baseline["importers"] = json!({".":{},"app":{},"other":other});
    let mut candidate = Lockfile::parse(OURS).unwrap().project().unwrap().clone();
    let app = candidate["importers"]["."].clone();
    let unrelated = Lockfile::parse(THEIRS).unwrap().project().unwrap()["importers"]["."].clone();
    candidate["importers"] = json!({".":{},"app":app,"other":unrelated});
    // Include the unrelated candidate records, as a broad pnpm regeneration would.
    let extra = Lockfile::parse(THEIRS).unwrap();
    for section in ["packages", "snapshots"] {
        candidate[section].as_object_mut().unwrap().extend(
            extra.project().unwrap()[section]
                .as_object()
                .unwrap()
                .clone(),
        );
    }
    let before = json!({"name":"app","private":true});
    let after = json!({"name":"app","private":true,"dependencies":{"is-number":"7.0.0","is-odd":"3.0.1","is-even":"1.0.0"}});
    let baseline = Lockfile::parse(&baseline.to_string()).unwrap();
    let candidate = Lockfile::parse(&candidate.to_string()).unwrap();
    let output = converge(
        &baseline,
        &candidate,
        &[ManifestChange {
            importer: "app",
            before: Some(&before),
            after: Some(&after),
        }],
        locksmith::ConvergeOptions {
            baseline_configuration: None,
            configuration: Some(&locksmith::WorkspaceConfig {
                override_tie_break: override_tie_break(),
                ..Default::default()
            }),
            registry: None,
        },
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["other"],
        baseline.project().unwrap()["importers"]["other"]
    );
    assert!(
        output.project().unwrap()["snapshots"]
            .get("is-buffer@2.0.5")
            .is_none()
    );
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir_all(workspace.join("app")).unwrap();
    fs::create_dir_all(workspace.join("other")).unwrap();
    fs::write(
        workspace.join("package.json"),
        json!({"name":"convergence-workspace","private":true}).to_string(),
    )
    .unwrap();
    fs::write(
        workspace.join("pnpm-workspace.yaml"),
        "packages:\n  - app\n  - other\n",
    )
    .unwrap();
    fs::write(workspace.join("app/package.json"), after.to_string()).unwrap();
    fs::write(workspace.join("other/package.json"),json!({"name":"other","private":true,"dependencies":{"is-number":"7.0.0","is-odd":"3.0.1"}}).to_string()).unwrap();
    let rendered = output.to_yaml().unwrap();
    fs::write(workspace.join("pnpm-lock.yaml"), &rendered).unwrap();
    install(&workspace, &root.path().join("store"), true);
    assert_eq!(
        fs::read_to_string(workspace.join("pnpm-lock.yaml")).unwrap(),
        rendered
    );
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn generated_workspace_converges_peer_and_injected_packages_without_sibling_upgrades() {
    use locksmith::{ManifestChange, converge};
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let baseline_dir = root.path().join("baseline");
    let candidate_dir = root.path().join("candidate");
    let app_before = json!({"name":"app","private":true,"dependencies":{"lib":"workspace:*","is-number":"6.0.0","is-odd":"^3.0.0"},"dependenciesMeta":{"lib":{"injected":true}}});
    let lib_before = json!({"name":"lib","version":"1.0.0","peerDependencies":{"is-number":"*"},"dependencies":{"is-buffer":"1.1.6"}});
    let mut app_after = app_before.clone();
    app_after["dependencies"]["is-number"] = json!("7.0.0");
    let mut lib_after = lib_before.clone();
    lib_after["dependencies"]["is-buffer"] = json!("2.0.5");
    let write_workspace = |dir: &Path, app: &Value, lib: &Value| {
        fs::create_dir_all(dir.join("app")).unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("package.json"), "{\"private\":true}").unwrap();
        fs::write(dir.join(".npmrc"), "dedupe-injected-deps=false\n").unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            "packages:\n  - app\n  - lib\n",
        )
        .unwrap();
        fs::write(dir.join("app/package.json"), app.to_string()).unwrap();
        fs::write(dir.join("lib/package.json"), lib.to_string()).unwrap();
    };
    let mut pinned = app_before.clone();
    pinned["dependencies"]["is-odd"] = json!("3.0.0");
    write_workspace(&baseline_dir, &pinned, &lib_before);
    install(&baseline_dir, &store, false);
    fs::write(
        baseline_dir.join("app/package.json"),
        app_before.to_string(),
    )
    .unwrap();
    install(&baseline_dir, &store, false);
    let baseline =
        Lockfile::parse(&fs::read_to_string(baseline_dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    assert_eq!(
        baseline.project().unwrap()["importers"]["app"]["dependencies"]["is-odd"]["version"],
        "3.0.0"
    );
    write_workspace(&candidate_dir, &app_after, &lib_after);
    install(&candidate_dir, &store, false);
    let candidate =
        Lockfile::parse(&fs::read_to_string(candidate_dir.join("pnpm-lock.yaml")).unwrap())
            .unwrap();
    assert_eq!(
        candidate.project().unwrap()["importers"]["app"]["dependencies"]["is-odd"]["version"],
        "3.0.1"
    );
    let output = converge(
        &baseline,
        &candidate,
        &[
            ManifestChange {
                importer: "app",
                before: Some(&app_before),
                after: Some(&app_after),
            },
            ManifestChange {
                importer: "lib",
                before: Some(&lib_before),
                after: Some(&lib_after),
            },
        ],
        locksmith::ConvergeOptions {
            baseline_configuration: None,
            configuration: Some(&locksmith::WorkspaceConfig {
                override_tie_break: override_tie_break(),
                ..Default::default()
            }),
            registry: None,
        },
    )
    .unwrap();
    output.validate().unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["app"]["dependencies"]["is-odd"]["version"],
        "3.0.0"
    );
    let rendered = output.to_yaml().unwrap();
    fs::write(candidate_dir.join("pnpm-lock.yaml"), &rendered).unwrap();
    install(&candidate_dir, &store, true);
    assert_eq!(
        fs::read_to_string(candidate_dir.join("pnpm-lock.yaml")).unwrap(),
        rendered
    );
    let installed: Value = serde_json::from_str(
        &fs::read_to_string(candidate_dir.join("app/node_modules/is-odd/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(installed["version"], "3.0.0");
    // Resolve from the installed package using Node's public module resolution;
    // pnpm may put dependencies beside an injected package in its virtual store.
    let resolved = Command::new("node").args(["-e", r#"
        const fs = require('node:fs');
        const { createRequire } = require('node:module');
        const req = createRequire(fs.realpathSync(process.argv[1] + '/app/node_modules/lib/package.json'));
        console.log(JSON.stringify([req('is-number/package.json').version, req('is-buffer/package.json').version]));
    "#]).arg(&candidate_dir).output().unwrap();
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    let versions: Value = serde_json::from_slice(&resolved.stdout).unwrap();
    assert_eq!(versions, json!(["7.0.0", "2.0.5"]));
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn extension_edits_keep_baseline_pins_and_match_native_peer_instances() {
    use locksmith::{ConvergeOptions, ManifestChange, converge};

    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let prepare = |dir: &Path, extended: bool, stable: &str| {
        let selector = if extended {
            "is-odd@0.1.2"
        } else {
            "is-odd@0.1.1"
        };
        let config = json!({"packages":["apps/*"],"packageExtensions":{
            selector:{"dependencies":{"react-dom":"18.2.0"}}
        }});
        fs::create_dir_all(dir).unwrap();
        let root = json!({"name":"root","private":true,"pnpm":config});
        fs::write(dir.join("package.json"), root.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&config).unwrap(),
        )
        .unwrap();
        let mut manifests = vec![(".".to_owned(), root)];
        for (id, react) in [("one", "18.2.0"), ("two", "18.3.1")] {
            let path = format!("apps/{id}");
            let mut manifest = json!({"name":id,"private":true,"dependencies":{"is-even":"1.0.0","react":react,"stable":format!("npm:is-odd@{stable}")}});
            if extended && id == "one" {
                manifest["devDependencies"] = json!({"extra":"npm:react@18.3.1"});
            }
            fs::create_dir_all(dir.join(&path)).unwrap();
            fs::write(dir.join(&path).join("package.json"), manifest.to_string()).unwrap();
            manifests.push((path, manifest));
        }
        (config, manifests)
    };
    let build = |name: &str, extended: bool, seed: &str| {
        let dir = tmp.path().join(name);
        prepare(&dir, extended, seed);
        install(&dir, &store, false);
        let (config, manifests) = prepare(&dir, extended, "^3.0.0");
        install(&dir, &store, false);
        let lock =
            Lockfile::try_from(fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
        (super::cfg(&config), manifests, lock, dir)
    };
    let baseline = build("baseline", false, "3.0.0");
    let installed = build("installed", true, "3.0.1");
    let reference = build("reference", true, "3.0.0");
    let baseline_pin = |l: &Lockfile| {
        l.project().unwrap()["importers"]["apps/one"]["dependencies"]["stable"]["version"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_ne!(
        baseline_pin(&baseline.2),
        baseline_pin(&installed.2),
        "fixture must exercise unrelated drift"
    );
    for (before, after, expected) in [
        (&baseline, &installed, &reference),
        (&reference, &baseline, &baseline),
    ] {
        let changes: Vec<_> = before
            .1
            .iter()
            .zip(&after.1)
            .map(|((id, old), (_, new))| ManifestChange {
                importer: id,
                before: Some(old),
                after: Some(new),
            })
            .collect();
        let options = ConvergeOptions {
            baseline_configuration: Some(&before.0),
            configuration: Some(&after.0),
            registry: None,
        };
        let result = converge(&before.2, &after.2, &changes, options).unwrap();
        assert_eq!(result.project(), expected.2.project());
        let yaml = result.to_yaml().unwrap();
        assert_eq!(
            converge(&before.2, &result, &changes, options)
                .unwrap()
                .to_yaml()
                .unwrap(),
            yaml
        );
        fs::write(after.3.join("pnpm-lock.yaml"), yaml).unwrap();
        install(&after.3, &store, true);
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn workspace_rules_converge_to_the_native_graph_in_each_consumer() {
    use locksmith::{ConvergeOptions, ManifestChange, RepairContext, converge, repair};
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let build = |dir: &str, edited: bool, seed: &str| {
        let d = tmp.path().join(dir);
        let version = if edited { "7.0.0" } else { "6.0.0" };
        let mut cfg = json!({"packages":["apps/*"],"peersSuffixMaxLength":4,
            "catalogs":{"toolchain":{"is-number":version}},
            "overrides":{"is-odd@0.1.2>is-number":"$is-number"}});
        if edited {
            cfg["packageExtensions"] =
                json!({"is-even@1.0.0":{"peerDependencies":{"is-number":"^6 || ^7"}}});
        }
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&cfg).unwrap(),
        )
        .unwrap();
        if super::pnpm_version().major() == 9 {
            fs::write(d.join(".npmrc"), "peers-suffix-max-length=4\n").unwrap();
        }
        let mut manifests = vec![];
        for (id, provider) in [("a", "catalog:toolchain"), ("b", "6.0.0")] {
            let path = format!("apps/{id}");
            let pkg = json!({"name":id,"private":true,"dependencies":{"is-even":"1.0.0","is-number":provider}});
            fs::create_dir_all(d.join(&path)).unwrap();
            fs::write(d.join(&path).join("package.json"), pkg.to_string()).unwrap();
            manifests.push((path, pkg));
        }
        let mut root =
            json!({"private":true,"pnpm":cfg,"dependencies":{"kleur":seed,"is-number":version}});
        fs::write(d.join("package.json"), root.to_string()).unwrap();
        install(&d, &store, false);
        root["dependencies"]["kleur"] = "^4.1.4".into();
        fs::write(d.join("package.json"), root.to_string()).unwrap();
        install(&d, &store, false);
        manifests.push((".".to_owned(), root));
        let lock =
            Lockfile::try_from(fs::read_to_string(d.join("pnpm-lock.yaml")).unwrap()).unwrap();
        (super::cfg(&cfg), manifests, lock, d)
    };
    let base = build("base", false, "4.1.4");
    let fresh = build("fresh", true, "4.1.5");
    let reference = build("reference", true, "4.1.4");
    assert_ne!(
        base.2.project().unwrap()["importers"]["."]["dependencies"]["kleur"]["version"],
        fresh.2.project().unwrap()["importers"]["."]["dependencies"]["kleur"]["version"]
    );
    for (before, after, expected) in [(&base, &fresh, &reference), (&reference, &base, &base)] {
        let changes: Vec<_> = before
            .1
            .iter()
            .zip(&after.1)
            .map(|((id, old), (_, new))| ManifestChange {
                importer: id,
                before: Some(old),
                after: Some(new),
            })
            .collect();
        let options = ConvergeOptions {
            baseline_configuration: Some(&before.0),
            configuration: Some(&after.0),
            registry: None,
        };
        let result = converge(&before.2, &after.2, &changes, options).unwrap();
        assert_eq!(result.project(), expected.2.project());
        let raw = result.to_yaml().unwrap();
        let manifests = after.1.iter().cloned().collect();
        let context = RepairContext {
            manifests: &manifests,
            configuration: &after.0,
            patches: &Default::default(),
            parents: &[],
            registry: None,
        };
        assert_eq!(repair(&raw, &context).unwrap().to_yaml().unwrap(), raw);
        assert_eq!(
            converge(&before.2, &result, &changes, options)
                .unwrap()
                .to_yaml()
                .unwrap(),
            raw
        );
        fs::write(after.3.join("pnpm-lock.yaml"), raw).unwrap();
        install(&after.3, &store, true);
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn adding_a_peer_preserves_the_consumers_other_binding_in_a_new_hashed_instance() {
    use locksmith::{ConvergeOptions, ManifestChange, converge};
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let build = |name: &str, edited: bool, seed: &str| {
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        if super::pnpm_version().major() == 9 {
            fs::write(dir.join(".npmrc"), "peers-suffix-max-length=4\n").unwrap();
        }
        let mut peers = json!({"is-number":"^6 || ^7"});
        if edited {
            peers["kleur"] = "^4".into();
        }
        let cfg = json!({"packages":["."],"peersSuffixMaxLength":4,
            "packageExtensions":{"is-even@1.0.0":{"peerDependencies":peers}}});
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&cfg).unwrap(),
        )
        .unwrap();
        let mut pkg = json!({"name":"app","private":true,"pnpm":cfg,
            "dependencies":{"is-even":"1.0.0","is-number":seed,"kleur":"4.1.4"}});
        for spec in [seed, "^6 || ^7"] {
            pkg["dependencies"]["is-number"] = spec.into();
            fs::write(dir.join("package.json"), pkg.to_string()).unwrap();
            install(&dir, &store, false);
        }
        let lock =
            Lockfile::try_from(fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
        (super::cfg(&cfg), pkg, lock, dir)
    };
    let base = build("base", false, "6.0.0");
    let fresh = build("fresh", true, "7.0.0");
    let expected = build("reference", true, "6.0.0");
    let plugin = |l: &Lockfile| {
        l.project().unwrap()["importers"]["."]["dependencies"]["is-even"]["version"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        expected.2.project().unwrap()["settings"]["peersSuffixMaxLength"],
        4
    );
    assert!(!plugin(&expected.2).contains("kleur@"));
    assert_ne!(plugin(&base.2), plugin(&expected.2));
    assert_ne!(plugin(&fresh.2), plugin(&expected.2));
    let opts = ConvergeOptions {
        baseline_configuration: Some(&base.0),
        configuration: Some(&fresh.0),
        registry: None,
    };
    let changes = [ManifestChange {
        importer: ".",
        before: Some(&base.1),
        after: Some(&fresh.1),
    }];
    let result = converge(&base.2, &fresh.2, &changes, opts).unwrap();
    assert_eq!(result.project(), expected.2.project());
    let output = result.to_yaml().unwrap();
    assert_eq!(
        converge(&base.2, &result, &changes, opts)
            .unwrap()
            .to_yaml()
            .unwrap(),
        output
    );
    fs::write(fresh.3.join("pnpm-lock.yaml"), output).unwrap();
    install(&fresh.3, &store, true);
}
