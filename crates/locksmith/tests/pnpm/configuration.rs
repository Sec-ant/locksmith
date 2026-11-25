use super::{cfg, install, override_tie_break};
use locksmith::{Lockfile, LockfileError, WorkspaceConfig};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn mixed_optional_rules_follow_native_resolution_and_frozen_validation() {
    use locksmith::{MergeContext, MergeTarget, RepairContext, merge, repair};
    use std::collections::BTreeMap;

    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let raw = json!({"packages":[],"ignoredOptionalDependencies":["*","!is-number"]});
    let config = cfg(&raw);
    let manifest = json!({"name":"test-root","private":true,"pnpm":raw,
        "dependencies":{"yocto-queue":"0.1.0"},"devDependencies":{"is-buffer":"2.0.5"},
        "optionalDependencies":{"is-number":"7.0.0","is-buffer":"1.1.6"}});
    let prepare = |name: &str| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(dir.join("pnpm-workspace.yaml"), raw.to_string()).unwrap();
        dir
    };
    let fresh = prepare("fresh");
    let frozen = prepare("frozen");
    install(&fresh, &store, false);
    let text = fs::read_to_string(fresh.join("pnpm-lock.yaml")).unwrap();
    fs::write(frozen.join("pnpm-lock.yaml"), &text).unwrap();
    let native = super::run_install(&frozen, &store, true);
    if !native.status.success() {
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&native.stdout),
            String::from_utf8_lossy(&native.stderr)
        );
        assert!(diagnostic.contains("OUTDATED_LOCKFILE"), "{diagnostic}");
    }
    let lock = Lockfile::parse(&text).unwrap();
    let manifests = BTreeMap::from([(".".into(), manifest)]);
    let patches = BTreeMap::new();
    let validation =
        lock.validate_workspace(&manifests, &Default::default(), &config, &patches, |_| {
            Ok(None)
        });
    assert_eq!(
        validation.is_ok(),
        native.status.success(),
        "{validation:?}"
    );
    let context = MergeContext {
        target: MergeTarget::Ours,
        manifests: &manifests,
        configuration: &config,
        configurations: [&config; 3],
        patches: &patches,
        registry: None,
    };
    let merged = merge(&lock, &lock, &lock, Some(&context)).unwrap();
    let repaired = repair(
        &text,
        &RepairContext {
            manifests: &manifests,
            configuration: &config,
            patches: &patches,
            parents: &[],
            registry: None,
        },
    )
    .unwrap();
    for result in [merged, repaired] {
        for section in ["ignoredOptionalDependencies", "importers", "snapshots"] {
            assert_eq!(
                result.project().unwrap()[section],
                lock.project().unwrap()[section],
                "{section}"
            );
        }
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn optional_extension_exclusions_match_a_clean_install_in_merge_and_repair() {
    use locksmith::{MergeContext, MergeTarget, RepairContext, merge, repair};
    use std::collections::BTreeMap;

    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"packages":[],"ignoredOptionalDependencies":["is-buf*"]});
    let mut after = before.clone();
    after["packageExtensions"] =
        json!({"is-number@7":{"optionalDependencies":{"is-buffer":"2.0.5"}}});
    let old = cfg(&before);
    let final_config = cfg(&after);
    let prepare = |name: &str, parent: bool, settings: &Value| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        let mut manifest =
            json!({"private":true,"pnpm":settings,"dependencies":{"is-buffer":"2.0.5"}});
        if parent {
            manifest["dependencies"]["is-number"] = json!("7.0.0");
        }
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(dir.join("pnpm-workspace.yaml"), settings.to_string()).unwrap();
        install(&dir, &store, false);
        let text = fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap();
        (manifest, Lockfile::parse(&text).unwrap())
    };
    let (_, base) = prepare("base", false, &before);
    let (_, ours) = prepare("ours", true, &before);
    let (_, theirs) = prepare("theirs", false, &after);
    let (manifest, native) = prepare("native", true, &after);
    let manifests = BTreeMap::from([(".".into(), manifest)]);
    let patches = BTreeMap::new();
    let ctx = MergeContext {
        target: MergeTarget::Ours,
        manifests: &manifests,
        configuration: &final_config,
        configurations: [&old, &old, &final_config],
        patches: &patches,
        registry: None,
    };
    let merged = merge(&base, &ours, &theirs, Some(&ctx)).unwrap();
    let expected = native.project().unwrap();
    let mut damaged = expected.clone();
    damaged["snapshots"]
        .as_object_mut()
        .unwrap()
        .remove("is-number@7.0.0");
    let repaired = repair(
        &damaged.to_string(),
        &RepairContext {
            manifests: &manifests,
            configuration: &final_config,
            parents: &[(&ours, &old)],
            patches: &patches,
            registry: None,
        },
    )
    .unwrap();
    for result in [&merged, &repaired] {
        assert_eq!(
            result.project().unwrap()["snapshots"],
            expected["snapshots"]
        );
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn changed_peer_overrides_match_fresh_resolution() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    for explicit_provider in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let prepare = |name: &str, dom: &str, react: &str| {
            let dir = root.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            let configuration =
                json!({"packages":[],"dedupePeers":false,"overrides":{"react-dom>react":react}});
            let mut manifest = json!({"name":"consumer","private":true,"dependencies":{"react-dom":dom},"pnpm":configuration});
            if explicit_provider {
                manifest["dependencies"]["react"] = json!("18.2.0");
            }
            fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
            fs::write(
                dir.join("pnpm-workspace.yaml"),
                serde_saphyr::to_string(&configuration).unwrap(),
            )
            .unwrap();
            (dir, manifest, cfg(&configuration))
        };
        let (base, _, before) = prepare("base", "18.2.0", "18.2.0");
        let (ours, _, after) = prepare("ours", "18.2.0", "18.3.1");
        let (theirs, _, _) = prepare("theirs", "18.3.1", "18.2.0");
        let (expected, _, _) = prepare("expected", "18.3.1", "18.3.1");
        let locks = [&base, &ours, &theirs, &expected].map(|dir| {
            install(dir, &store, false);
            Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
        });
        let (dir, manifest, _) = prepare("merged", "18.3.1", "18.3.1");
        let result = merge(
            &locks[0],
            &locks[1],
            &locks[2],
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &BTreeMap::from([(".".into(), manifest)]),
                configurations: [&before, &after, &before],
                configuration: &after,
                patches: &BTreeMap::new(),
            }),
        )
        .unwrap();
        for section in ["importers", "packages", "snapshots"] {
            assert_eq!(
                result.project().unwrap()[section],
                locks[3].project().unwrap()[section],
                "{section}, explicit provider: {explicit_provider}"
            );
        }
        fs::write(dir.join("pnpm-lock.yaml"), result.to_yaml().unwrap()).unwrap();
        install(&dir, &store, true);
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn peer_consumers_keep_provider_extensions_after_configuration_changes() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let prepare = |name: &str, dom: &str, upgraded: bool| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut configuration = json!({"packages":[],"dedupePeers":false,
            "overrides":{"react":"18.2.0"}});
        if upgraded {
            configuration["packageExtensions"] =
                json!({"react@18.2.0":{"peerDependencies":{"is-number":"7.0.0"}}});
        }
        let manifest = json!({"name":"consumer","private":true,
            "dependencies":{"react-dom":dom,"is-number":"7.0.0"},"pnpm":configuration});
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&configuration).unwrap(),
        )
        .unwrap();
        (dir, manifest, cfg(&configuration))
    };
    let (base, _, before) = prepare("base", "18.2.0", false);
    let (ours, _, after) = prepare("ours", "18.2.0", true);
    let (theirs, _, _) = prepare("theirs", "18.3.1", false);
    let (expected, _, _) = prepare("expected", "18.3.1", true);
    let locks = [&base, &ours, &theirs, &expected].map(|dir| {
        install(dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    });
    let (dir, manifest, _) = prepare("merged", "18.3.1", true);
    let result = merge(
        &locks[0],
        &locks[1],
        &locks[2],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &BTreeMap::from([(".".into(), manifest)]),
            configurations: [&before, &after, &before],
            configuration: &after,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    assert_eq!(
        result.project().unwrap()["importers"]["."]["dependencies"]["react-dom"],
        locks[3].project().unwrap()["importers"]["."]["dependencies"]["react-dom"]
    );
    fs::write(dir.join("pnpm-lock.yaml"), result.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
    let installed = Command::new("node").args(["-e", r#"
        const fs = require('node:fs');
        const {createRequire} = require('node:module');
        const dom = createRequire(fs.realpathSync(process.argv[1] + '/node_modules/react-dom/package.json'));
        const react = createRequire(dom.resolve('react/package.json'));
        console.log(JSON.stringify([dom('react/package.json').version, react('is-number/package.json').version]));
    "#]).arg(&dir).output().unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&installed.stdout).unwrap(),
        json!(["18.2.0", "7.0.0"])
    );
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn removed_transitive_patch_reuses_the_original_package_artifact() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"patchedDependencies":{"is-number@6.0.0":"number.patch"}});
    let prepare = |name: &str, patched: bool| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut manifest =
            json!({"name":"patch-removal-test","private":true,"dependencies":{"is-odd":"3.0.1"}});
        if patched {
            manifest["pnpm"] = before.clone();
            fs::write(dir.join("number.patch"), "diff --git a/locksmith-marker.txt b/locksmith-marker.txt\nnew file mode 100644\nindex 0000000..e69de29\n--- /dev/null\n+++ b/locksmith-marker.txt\n@@ -0,0 +1 @@\n+patched\n").unwrap();
        }
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            if patched {
                "packages: []\npatchedDependencies:\n  is-number@6.0.0: number.patch\n"
            } else {
                "packages: []\n"
            },
        )
        .unwrap();
        (dir, manifest)
    };
    let marker = |dir: &Path| {
        fs::canonicalize(dir.join("node_modules/is-odd"))
            .unwrap()
            .parent()
            .unwrap()
            .join("is-number/locksmith-marker.txt")
    };
    let (source, _) = prepare("source", true);
    install(&source, &store, false);
    assert!(marker(&source).exists());
    let input =
        Lockfile::parse(&fs::read_to_string(source.join("pnpm-lock.yaml")).unwrap()).unwrap();
    let (merged, manifest) = prepare("merged", false);
    let old = WorkspaceConfig::from_pnpm(&before).unwrap();
    let final_config = WorkspaceConfig::default();
    let output = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &BTreeMap::from([(".".into(), manifest)]),
            configurations: [&old; 3],
            configuration: &final_config,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    fs::write(merged.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&merged, &store, true);
    assert!(!marker(&merged).exists());
    let (native, _) = prepare("native", false);
    install(&native, &store, false);
    let expected =
        Lockfile::parse(&fs::read_to_string(native.join("pnpm-lock.yaml")).unwrap()).unwrap();
    for section in ["importers", "packages", "snapshots"] {
        assert_eq!(
            output.project().unwrap()[section],
            expected.project().unwrap()[section],
            "{section}"
        );
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn generated_override_specifiers_match_raw_manifests() {
    use locksmith::{ManifestChange, converge};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("workspace");
    fs::create_dir(&dir).unwrap();
    // pnpm 11+ reads this configuration from pnpm-workspace.yaml; older
    // versions also accept the identical package.json pnpm setting below.
    fs::write(
        dir.join("pnpm-workspace.yaml"),
        "packages:\n  - .\noverrides:\n  is-number: 7.0.0\n",
    )
    .unwrap();
    let before = json!({"name":"override-test","private":true,"dependencies":{"is-number":"6.0.0"},"pnpm":{"overrides":{"is-number":"7.0.0"}}});
    let mut after = before.clone();
    after["dependencies"]["is-odd"] = json!("3.0.1");
    fs::write(dir.join("package.json"), before.to_string()).unwrap();
    install(&dir, &root.path().join("store"), false);
    let baseline =
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    fs::write(dir.join("package.json"), after.to_string()).unwrap();
    install(&dir, &root.path().join("store"), false);
    let candidate =
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    let output = converge(
        &baseline,
        &candidate,
        &[ManifestChange {
            importer: ".",
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
        output.project().unwrap()["importers"]["."]["dependencies"]["is-number"]["specifier"],
        "7.0.0"
    );
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &root.path().join("store"), true);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn generated_override_upgrade_and_new_consumer_require_resolution() {
    let root = tempfile::tempdir().unwrap();
    let generate = |name: &str, version: &str, add_consumer: bool| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            format!("packages:\n  - packages/*\noverrides:\n  is-number: {version}\n"),
        )
        .unwrap();
        fs::write(
            dir.join("package.json"),
            json!({"name":"override-merge-test","private":true,
                "dependencies":{"is-number":"*"},
                "pnpm":{"overrides":{"is-number":version}}})
            .to_string(),
        )
        .unwrap();
        if add_consumer {
            fs::create_dir_all(dir.join("packages/app")).unwrap();
            fs::write(
                dir.join("packages/app/package.json"),
                json!({"name":"new-consumer","private":true,"dependencies":{"is-number":"*"}})
                    .to_string(),
            )
            .unwrap();
        }
        install(&dir, &root.path().join("store"), false);
        let lock =
            Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
        lock.validate().unwrap();
        lock
    };
    let base = generate("base", "6.0.0", false);
    let ours = generate("ours", "7.0.0", false);
    let theirs = generate("theirs", "6.0.0", true);
    assert!(matches!(
        locksmith::merge(&base, &ours, &theirs, None),
        Err(LockfileError::InvalidGraph(_))
    ));
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn unrelated_package_extensions_preserve_both_branches_changes() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"packages":["packages/*"]});
    let after = json!({"packages":["packages/*"],"packageExtensions":{"is-odd@3":{"dependencies":{"is-buffer":"2.0.5"}}}});
    let prepare = |name: &str, number: &str, config: &Value| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let manifest = json!({"name":"extension-test","private":true,"pnpm":config,"dependencies":{"is-number":number,"is-odd":"3.0.1"}});
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(config).unwrap(),
        )
        .unwrap();
        (dir, manifest)
    };
    let mut locks = Vec::new();
    for (name, number, config) in [
        ("base", "6.0.0", &before),
        ("ours", "7.0.0", &before),
        ("theirs", "6.0.0", &after),
    ] {
        let (dir, _) = prepare(name, number, config);
        install(&dir, &store, false);
        locks.push(
            Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap(),
        );
    }
    let (dir, manifest) = prepare("merged", "7.0.0", &after);
    let manifests = BTreeMap::from([(".".into(), manifest)]);
    let patches = BTreeMap::new();
    let result = merge(
        &locks[0],
        &locks[1],
        &locks[2],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before), &cfg(&before), &cfg(&after)],
            configuration: &cfg(&after),
            patches: &patches,
        }),
    )
    .unwrap();
    let rendered = result.to_yaml().unwrap();
    fs::write(dir.join("pnpm-lock.yaml"), &rendered).unwrap();
    install(&dir, &store, true);
    assert_eq!(
        fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap(),
        rendered
    );
    let installed = Command::new("node").current_dir(&dir).args(["-e", "const r=require('module').createRequire(require.resolve('is-odd')); console.log(JSON.stringify([require('is-number/package.json').version,r('is-buffer/package.json').version]))"]).output().unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&installed.stdout).unwrap(),
        json!(["7.0.0", "2.0.5"])
    );
}

#[test]
#[ignore = "requires PNPM_BIN"]
fn independently_added_package_extensions_pass_frozen_install() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"packages":["packages/*"],"packageExtensions":{"unused@1":{"peerDependencies":{"a":"^1"}}}});
    let mut ours = before.clone();
    ours["packageExtensions"]["unused@2"] = json!({"peerDependenciesMeta":{"b":{"optional":true}}});
    let mut theirs = before.clone();
    theirs["packageExtensions"]["other@1"] = json!({"dependencies":{"c":"2"}});
    let mut final_config = ours.clone();
    final_config["packageExtensions"]["other@1"] = theirs["packageExtensions"]["other@1"].clone();
    let prepare = |name: &str, config: &Value| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let manifest = json!({"private":true,"pnpm":config});
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(config).unwrap(),
        )
        .unwrap();
        (dir, manifest)
    };
    let mut locks = Vec::new();
    for (name, config) in [("base", &before), ("ours", &ours), ("theirs", &theirs)] {
        let (dir, _) = prepare(name, config);
        install(&dir, &store, false);
        locks.push(
            Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap(),
        );
    }
    let (dir, manifest) = prepare("merged", &final_config);
    let manifests = BTreeMap::from([(".".into(), manifest)]);
    let patches = BTreeMap::new();
    let output = merge(
        &locks[0],
        &locks[1],
        &locks[2],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before), &cfg(&ours), &cfg(&theirs)],
            configuration: &cfg(&final_config),
            patches: &patches,
        }),
    )
    .unwrap();
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn reordered_overrides_merge_without_a_frozen_install_rewrite() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"packages":["packages/*"],"overrides":{"is-number@^7":"5.0.0","is-number@7":"6.0.0"}});
    let after = json!({"packages":["packages/*"],"overrides":{"is-number@7":"6.0.0","is-number@^7":"5.0.0"}});
    let prepare = |name: &str, config: &Value| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let manifest = json!({"private":true,"dependencies":{"is-number":"^7.0.0"},"pnpm":config});
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(config).unwrap(),
        )
        .unwrap();
        (dir, manifest)
    };
    let (base_dir, _) = prepare("base", &before);
    let (ours_dir, _) = prepare("ours", &after);
    install(&base_dir, &store, false);
    install(&ours_dir, &store, false);
    let base =
        Lockfile::parse(&fs::read_to_string(base_dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    let ours =
        Lockfile::parse(&fs::read_to_string(ours_dir.join("pnpm-lock.yaml")).unwrap()).unwrap();
    let (base_version, ours_version) = match override_tie_break() {
        locksmith::OverrideTieBreak::Last => ("6.0.0", "5.0.0"),
        locksmith::OverrideTieBreak::First => ("5.0.0", "6.0.0"),
    };
    assert_eq!(
        base.project().unwrap()["importers"]["."]["dependencies"]["is-number"]["version"],
        base_version
    );
    assert_eq!(
        ours.project().unwrap()["importers"]["."]["dependencies"]["is-number"]["version"],
        ours_version
    );
    let (dir, manifest) = prepare("merged", &after);
    for (left, right) in [(&base, &ours), (&ours, &base)] {
        let plain = merge(&base, left, right, None).unwrap().to_yaml().unwrap();
        assert_eq!(plain, ours.to_yaml().unwrap());
        fs::write(dir.join("pnpm-lock.yaml"), plain).unwrap();
        install(&dir, &store, true);
    }
    let converged = locksmith::converge(
        &ours,
        &ours,
        &[locksmith::ManifestChange {
            importer: ".",
            before: Some(&manifest),
            after: Some(&manifest),
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
    assert_eq!(converged.to_yaml().unwrap(), ours.to_yaml().unwrap());
    let manifests = BTreeMap::from([(".".into(), manifest)]);
    let patches = BTreeMap::new();
    let output = merge(
        &base,
        &ours,
        &base,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before), &cfg(&after), &cfg(&before)],
            configuration: &cfg(&after),
            patches: &patches,
        }),
    )
    .unwrap();
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn override_upgrade_introduces_a_transitive_peer_under_the_final_provider() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let prepare = |name: &str, types: &str, zustand: &str| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let config = json!({"packages":["packages/*"],"overrides":{"zustand":zustand}});
        let manifest = json!({"private":true,"dependencies":{
            "@textea/json-viewer":"3.0.0","@types/react":types,"immer":"10.1.1",
            "@emotion/react":"11.14.0","@emotion/styled":"11.14.0","@mui/material":"5.16.11",
            "react":"18.2.0","react-dom":"18.2.0"
        },"pnpm":config});
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&config).unwrap(),
        )
        .unwrap();
        (dir, manifest, cfg(&config))
    };
    let (base_dir, _, before) = prepare("base", "18.0.26", "4.3.8");
    let (ours_dir, _, after) = prepare("ours", "18.0.26", "4.5.7");
    let (theirs_dir, _, _) = prepare("theirs", "18.2.0", "4.3.8");
    let (expected_dir, _, _) = prepare("expected", "18.2.0", "4.5.7");
    let locks = [&base_dir, &ours_dir, &theirs_dir, &expected_dir].map(|dir| {
        install(dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    });
    let (dir, manifest, _) = prepare("merged", "18.2.0", "4.5.7");
    let manifests = BTreeMap::from([(".".into(), manifest)]);
    let patches = BTreeMap::new();
    let output = merge(
        &locks[0],
        &locks[1],
        &locks[2],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&before, &after, &before],
            configuration: &after,
            patches: &patches,
        }),
    )
    .unwrap();
    let expected = locks[3].project().unwrap();
    let expected_version =
        expected["importers"]["."]["dependencies"]["@textea/json-viewer"]["version"]
            .as_str()
            .unwrap();
    let data = output.project().unwrap();
    assert_eq!(
        data["importers"]["."]["dependencies"]["@textea/json-viewer"]["version"],
        expected_version
    );
    let parent = format!("@textea/json-viewer@{expected_version}");
    assert_eq!(data["snapshots"][&parent], expected["snapshots"][&parent]);
    let child = format!(
        "zustand@{}",
        expected["snapshots"][&parent]["dependencies"]["zustand"]
            .as_str()
            .unwrap()
    );
    assert_eq!(data["snapshots"][&child], expected["snapshots"][&child]);
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn new_override_peers_propagate_through_an_injected_workspace_without_a_parent_witness() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let prepare = |name: &str, types: &str, zustand: &str, wrapper: bool| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".npmrc"), "dedupe-injected-deps=false\n").unwrap();
        let config = json!({"packages":["packages/*"],"dedupeInjectedDeps":false,"overrides":{"zustand":zustand}});
        let mut manifest = json!({"private":true,"dependencies":{
            "@types/react":types,"immer":"10.1.1",
            "@emotion/react":"11.14.0","@emotion/styled":"11.14.0","@mui/material":"5.16.11",
            "react":"18.2.0","react-dom":"18.2.0"
        },"pnpm":config});
        let mut manifests = BTreeMap::new();
        if wrapper {
            manifest["dependencies"]["locksmith-wrapper"] = json!("workspace:*");
            manifest["dependenciesMeta"] = json!({"locksmith-wrapper":{"injected":true}});
            let package = json!({"name":"locksmith-wrapper","version":"1.0.0","dependencies":{"@textea/json-viewer":"3.0.0"}});
            fs::create_dir_all(dir.join("packages/wrapper")).unwrap();
            fs::write(
                dir.join("packages/wrapper/package.json"),
                package.to_string(),
            )
            .unwrap();
            manifests.insert("packages/wrapper".into(), package);
        } else {
            manifest["dependencies"]["zustand"] = json!(zustand);
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
    let (base_dir, _, before) = prepare("base", "18.0.26", "4.3.8", true);
    let (ours_dir, _, after) = prepare("ours", "18.0.26", "4.5.7", false);
    let (theirs_dir, _, _) = prepare("theirs", "18.2.0", "4.3.8", true);
    let (expected_dir, _, _) = prepare("expected", "18.2.0", "4.5.7", true);
    let locks = [&base_dir, &ours_dir, &theirs_dir, &expected_dir].map(|dir| {
        install(dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    });
    let (dir, manifests, _) = prepare("merged", "18.2.0", "4.5.7", true);
    let patches = BTreeMap::new();
    let output = merge(
        &locks[0],
        &locks[1],
        &locks[2],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&before, &after, &before],
            configuration: &after,
            patches: &patches,
        }),
    )
    .unwrap();
    let expected = locks[3].project().unwrap();
    let actual = output.project().unwrap();
    let version = expected["importers"]["."]["dependencies"]["locksmith-wrapper"]["version"]
        .as_str()
        .unwrap();
    assert_eq!(
        actual["importers"]["."]["dependencies"]["locksmith-wrapper"]["version"],
        version
    );
    let wrapper = format!("locksmith-wrapper@{version}");
    let wrapper = expected["snapshots"]
        .as_object()
        .unwrap()
        .get_key_value(&wrapper)
        .or_else(|| {
            expected["snapshots"]
                .as_object()
                .unwrap()
                .get_key_value(version)
        })
        .unwrap_or_else(|| {
            panic!(
                "injected workspace snapshot for {version}: {:?}",
                expected["snapshots"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter(|key| key.contains("wrapper"))
                    .collect::<Vec<_>>()
            )
        })
        .0;
    assert_eq!(actual["snapshots"][wrapper], expected["snapshots"][wrapper]);
    let parent = format!(
        "@textea/json-viewer@{}",
        expected["snapshots"][wrapper]["dependencies"]["@textea/json-viewer"]
            .as_str()
            .unwrap()
    );
    assert_eq!(actual["snapshots"][&parent], expected["snapshots"][&parent]);
    let child = format!(
        "zustand@{}",
        expected["snapshots"][&parent]["dependencies"]["zustand"]
            .as_str()
            .unwrap()
    );
    assert_eq!(actual["snapshots"][&child], expected["snapshots"][&child]);
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn new_and_changed_patches_match_fresh_resolution_and_installed_contents() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let prepare = |name: &str, revision: Option<u8>| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut configuration = json!({"packages":[]});
        let mut patches = BTreeMap::new();
        if let Some(revision) = revision {
            configuration["patchedDependencies"] = json!({
                "react@18.2.0":"react.patch",
                "react-dom@18.2.0":"dom.patch",
                "is-number@6.0.0":"number.patch"
            });
            for path in ["react.patch", "dom.patch", "number.patch"] {
                let contents = format!(
                    "diff --git a/locksmith-marker.txt b/locksmith-marker.txt\nnew file mode 100644\nindex 0000000..e69de29\n--- /dev/null\n+++ b/locksmith-marker.txt\n@@ -0,0 +1 @@\n+revision {revision}\n"
                );
                fs::write(dir.join(path), &contents).unwrap();
                patches.insert(path.into(), contents);
            }
        }
        let manifest = json!({"name":"patch-transition-test","private":true,
            "dependencies":{"react":"18.2.0","react-dom":"18.2.0","is-odd":"3.0.1"},
            "pnpm":configuration});
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&configuration).unwrap(),
        )
        .unwrap();
        (dir, manifest, cfg(&configuration), patches)
    };
    let (source, _, mut before, _) = prepare("source", None);
    install(&source, &store, false);
    let mut input =
        Lockfile::parse(&fs::read_to_string(source.join("pnpm-lock.yaml")).unwrap()).unwrap();
    for (index, revision) in [Some(1), Some(2), None].into_iter().enumerate() {
        let (dir, manifest, configuration, patches) = prepare(&format!("merged-{index}"), revision);
        let output = merge(
            &input,
            &input,
            &input,
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Ours,
                manifests: &BTreeMap::from([(".".into(), manifest)]),
                configurations: [&before; 3],
                configuration: &configuration,
                patches: &patches,
            }),
        )
        .unwrap();
        fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
        install(&dir, &store, true);
        for package in ["react", "react-dom", "is-number"] {
            let installed = if package == "is-number" {
                fs::canonicalize(dir.join("node_modules/is-odd"))
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("is-number")
            } else {
                fs::canonicalize(dir.join("node_modules").join(package)).unwrap()
            };
            let marker = installed.join("locksmith-marker.txt");
            if let Some(revision) = revision {
                assert_eq!(
                    fs::read_to_string(marker).unwrap(),
                    format!("revision {revision}\n")
                );
            } else {
                assert!(!marker.exists());
            }
        }
        let (native, _, _, _) = prepare(&format!("native-{index}"), revision);
        install(&native, &store, false);
        let expected =
            Lockfile::parse(&fs::read_to_string(native.join("pnpm-lock.yaml")).unwrap()).unwrap();
        for section in ["patchedDependencies", "importers", "packages", "snapshots"] {
            assert_eq!(
                output.project().unwrap()[section],
                expected.project().unwrap()[section],
                "{section}, revision {revision:?}"
            );
        }
        input = output;
        before = configuration;
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn added_extensions_match_fresh_resolution_with_transitive_peers() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    for group in ["dependencies", "optionalDependencies"] {
        let before = json!({"packages":[],"packageExtensions":{"unused-locksmith-package":{"dependencies":{"is-number":"6.0.0"}}}});
        let mut after = before.clone();
        after["packageExtensions"]["is-number@6.0.0"] = json!({group:{"react-dom":"18.2.0"}});
        let prepare = |name: &str, parent: bool, react: &str, configuration: &Value| {
            let dir = root.path().join(format!("{group}-{name}"));
            fs::create_dir_all(&dir).unwrap();
            let mut manifest = json!({"name":"extension-composition-test","private":true,"dependencies":{"react":react},"pnpm":configuration});
            if parent {
                manifest["dependencies"]["is-number"] = json!("6.0.0");
            } else {
                manifest["dependencies"]["react-dom"] = json!("18.2.0");
            }
            fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
            fs::write(
                dir.join("pnpm-workspace.yaml"),
                serde_saphyr::to_string(configuration).unwrap(),
            )
            .unwrap();
            (dir, manifest, cfg(configuration))
        };
        let (base, _, old) = prepare("base", true, "18.2.0", &before);
        let (ours, _, _) = prepare("ours", true, "18.3.1", &before);
        let (theirs, _, final_config) = prepare("theirs", false, "18.2.0", &after);
        let (native, _, _) = prepare("native", true, "18.3.1", &after);
        let locks = [&base, &ours, &theirs, &native].map(|dir| {
            install(dir, &store, false);
            Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
        });
        let (dir, manifest, _) = prepare("merged", true, "18.3.1", &after);
        let output = merge(
            &locks[0],
            &locks[1],
            &locks[2],
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Ours,
                manifests: &BTreeMap::from([(".".into(), manifest)]),
                configurations: [&old, &old, &final_config],
                configuration: &final_config,
                patches: &BTreeMap::new(),
            }),
        )
        .unwrap();
        let expected = locks[3].project().unwrap();
        for section in [
            "packageExtensionsChecksum",
            "importers",
            "packages",
            "snapshots",
        ] {
            assert_eq!(
                output.project().unwrap()[section],
                expected[section],
                "{section}, {group}"
            );
        }
        fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
        install(&dir, &store, true);
        let parent = fs::canonicalize(dir.join("node_modules/is-number")).unwrap();
        let dom = fs::canonicalize(parent.parent().unwrap().join("react-dom")).unwrap();
        let provider: Value = serde_json::from_str(
            &fs::read_to_string(dom.parent().unwrap().join("react/package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(provider["version"], "18.3.1");
    }
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn added_extensions_replace_enclosing_peers_with_local_providers() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"packages":[],"packageExtensions":{"is-number@6.0.0":{"dependencies":{"react-dom":"18.2.0"}}}});
    let mut after = before.clone();
    after["packageExtensions"]["is-number@6.0.0"]["dependencies"]["react"] = json!("18.3.1");
    let prepare = |name: &str, parent: bool, configuration: &Value| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = json!({"name":"extension-provider-test","private":true,"dependencies":{"react":if parent {"18.2.0"} else {"18.3.1"}},"pnpm":configuration});
        if parent {
            manifest["dependencies"]["is-number"] = json!("6.0.0");
        } else {
            manifest["dependencies"]["react-dom"] = json!("18.2.0");
        }
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(configuration).unwrap(),
        )
        .unwrap();
        (dir, manifest, cfg(configuration))
    };
    let (base, _, old) = prepare("base", true, &before);
    let (donor, _, final_config) = prepare("donor", false, &after);
    let (native, _, _) = prepare("native", true, &after);
    let locks = [&base, &donor, &native].map(|dir| {
        install(dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    });
    let (dir, manifest, _) = prepare("merged", true, &after);
    let output = merge(
        &locks[0],
        &locks[0],
        &locks[1],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &BTreeMap::from([(".".into(), manifest)]),
            configurations: [&old, &old, &final_config],
            configuration: &final_config,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    for section in [
        "packageExtensionsChecksum",
        "importers",
        "packages",
        "snapshots",
    ] {
        assert_eq!(
            output.project().unwrap()[section],
            locks[2].project().unwrap()[section],
            "{section}"
        );
    }
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
    let parent = fs::canonicalize(dir.join("node_modules/is-number")).unwrap();
    let provider: Value = serde_json::from_str(
        &fs::read_to_string(parent.parent().unwrap().join("react/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(provider["version"], "18.3.1");
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn removed_overrides_recover_extensions_before_rebinding_new_peers() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"packages":[],"packageExtensions":{"is-number@6.0.0":{"dependencies":{"react-dom":"18.2.0"}}},"overrides":{"is-number>react-dom":"18.3.1"}});
    let mut after = before.clone();
    after.as_object_mut().unwrap().remove("overrides");
    let prepare = |name: &str, configuration: &Value, witness: bool| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = json!({"name":"extension-declaration-test","private":true,"dependencies":{"is-number":"6.0.0","react":"18.3.1"},"pnpm":configuration});
        if witness {
            manifest["dependencies"]["react-dom"] = json!("18.2.0");
        }
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(configuration).unwrap(),
        )
        .unwrap();
        (dir, manifest, cfg(configuration))
    };
    let (ours, _, old) = prepare("ours", &before, false);
    let (theirs, _, witness) = prepare("theirs", &json!({"packages":[]}), true);
    let (native, _, final_config) = prepare("native", &after, false);
    let locks = [&ours, &theirs, &native].map(|dir| {
        install(dir, &store, false);
        Lockfile::parse(&fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()).unwrap()
    });
    let (dir, manifest, _) = prepare("merged", &after, false);
    let result = merge(
        &locks[0],
        &locks[0],
        &locks[1],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &BTreeMap::from([(".".into(), manifest)]),
            configurations: [&old, &old, &witness],
            configuration: &final_config,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    for section in [
        "packageExtensionsChecksum",
        "importers",
        "packages",
        "snapshots",
    ] {
        assert_eq!(
            result.project().unwrap()[section],
            locks[2].project().unwrap()[section],
            "{section}"
        );
    }
    fs::write(dir.join("pnpm-lock.yaml"), result.to_yaml().unwrap()).unwrap();
    install(&dir, &store, true);
    let parent = fs::canonicalize(dir.join("node_modules/is-number")).unwrap();
    let dom = fs::canonicalize(parent.parent().unwrap().join("react-dom")).unwrap();
    let package: Value =
        serde_json::from_str(&fs::read_to_string(dom.join("package.json")).unwrap()).unwrap();
    let provider: Value = serde_json::from_str(
        &fs::read_to_string(dom.parent().unwrap().join("react/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(package["version"], "18.2.0");
    assert_eq!(provider["version"], "18.3.1");
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn catalog_range_overrides_and_publish_directory_match_a_fresh_install() {
    use locksmith::{
        ConvergeOptions, ManifestChange, MergeContext, MergeTarget, RepairContext, converge, merge,
        repair,
    };
    use std::collections::BTreeMap;

    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("workspace");
    let store = root.path().join("store");
    let settings = json!({
        "packages": ["apps/*"],
        "catalog": {"is-number":"^7"},
        "catalogs": {"legacy":{"is-number":"^5"}, "current":{"is-number":"^7"}},
        "overrides": {"is-number@7":"6.0.0"}
    });
    let mut manifests = BTreeMap::from([(
        ".".to_owned(),
        json!({"name":"root","private":true,"pnpm":{"overrides":settings["overrides"]}}),
    )]);
    for (name, specifier, publish) in [
        (
            "editor",
            "catalog:",
            json!({"directory":"dist","linkDirectory":false}),
        ),
        (
            "reader",
            "catalog:legacy",
            json!({"directory":"dist","linkDirectory":true}),
        ),
        ("viewer", "catalog:current", json!({"linkDirectory":false})),
    ] {
        manifests.insert(
            format!("apps/{name}"),
            json!({"name":name,"dependencies":{"is-number":specifier},"publishConfig":publish}),
        );
    }
    manifests.get_mut("apps/reader").unwrap()["dependencies"]["reader"] = json!("workspace:*");
    let viewer = &mut manifests.get_mut("apps/viewer").unwrap()["dependencies"];
    viewer["reader"] = json!("workspace:*");
    viewer["relative"] = json!("workspace:../reader");
    viewer["literal"] = json!("link:../reader");
    let snapshot = |path: &Path, projects: &BTreeMap<String, Value>| {
        fs::create_dir(path).unwrap();
        for (id, manifest) in projects {
            fs::create_dir_all(path.join(id)).unwrap();
            fs::write(path.join(id).join("package.json"), manifest.to_string()).unwrap();
        }
        fs::write(
            path.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(&settings).unwrap(),
        )
        .unwrap();
        install(path, &store, false);
        fs::read_to_string(path.join("pnpm-lock.yaml")).unwrap()
    };
    let mut previous = manifests.clone();
    previous.get_mut("apps/editor").unwrap()["publishConfig"]["linkDirectory"] = json!(true);
    previous.get_mut("apps/reader").unwrap()["publishConfig"]["linkDirectory"] = json!(false);
    let stale = snapshot(&root.path().join("before"), &previous);
    let native = snapshot(&dir, &manifests);
    let expected = Lockfile::parse(&native).unwrap();
    let config = cfg(&settings);
    let patches = BTreeMap::new();
    let context = RepairContext {
        manifests: &manifests,
        configuration: &config,
        patches: &patches,
        parents: &[],
        registry: None,
    };
    assert_eq!(
        repair(&native, &context).unwrap().to_yaml().unwrap(),
        native
    );
    let prior = Lockfile::parse(&stale).unwrap();
    let merged = merge(
        &prior,
        &prior,
        &prior,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configuration: &config,
            configurations: [&config; 3],
            patches: &patches,
            registry: None,
        }),
    )
    .unwrap();
    let repaired = repair(&stale, &context).unwrap();
    let changes = manifests
        .iter()
        .map(|(id, manifest)| ManifestChange {
            importer: id,
            before: previous.get(id),
            after: Some(manifest),
        })
        .collect::<Vec<_>>();
    let converged = converge(
        &prior,
        &expected,
        &changes,
        ConvergeOptions {
            baseline_configuration: Some(&config),
            configuration: Some(&config),
            registry: None,
        },
    )
    .unwrap();
    for lock in [merged, repaired, converged] {
        assert!(
            lock.dependency_closure("apps/viewer")
                .unwrap()
                .importers
                .contains("apps/reader")
        );
        for field in ["importers", "catalogs"] {
            assert_eq!(
                lock.project().unwrap()[field],
                expected.project().unwrap()[field]
            );
        }
        fs::write(dir.join("pnpm-lock.yaml"), lock.to_yaml().unwrap()).unwrap();
        install(&dir, &store, true);
    }
}
