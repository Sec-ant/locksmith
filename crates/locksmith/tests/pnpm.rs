//! Real package-manager differential tests (network and Git required).
//! `PNPM_BIN=/absolute/path/to/pnpm cargo test --test pnpm -- --ignored --nocapture`
use std::{env, fs, path::Path, process::Command, time::Instant};

use locksmith::{Lockfile, LockfileError};
use serde_json::{Value, json};

const BASE: &str = "lockfileVersion: '9.0'\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\nimporters:\n  .: {}\n";
const OURS: &str = include_str!("fixtures/pnpm10/ours.yaml");
const THEIRS: &str = include_str!("fixtures/pnpm10/theirs.yaml");

fn install(directory: &Path, store: &Path, frozen: bool) {
    let mut command =
        Command::new(env::var_os("PNPM_BIN").expect("set PNPM_BIN to the executable to test"));
    command
        .current_dir(directory)
        .env("CI", "true")
        .env("NO_UPDATE_NOTIFIER", "1")
        .args([
            "install",
            "--ignore-scripts",
            "--registry=https://registry.npmjs.org",
            "--config.manage-package-manager-versions=false",
            "--store-dir",
        ])
        .arg(store);
    if frozen {
        command.arg("--frozen-lockfile");
    } else {
        command.arg("--no-frozen-lockfile");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn workspace(path: &Path, lock: &str) {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join("pnpm-lock.yaml"), lock).unwrap();
    fs::write(
        path.join("package.json"),
        json!({"name":"locksmith-differential","private":true,"dependencies":{
            "is-number":"7.0.0","is-odd":"3.0.1","is-even":"1.0.0","is-buffer":"2.0.5"
        }})
        .to_string(),
    )
    .unwrap();
}

fn git_conflict(root: &Path, ours: &str, base: &str, theirs: &str) -> String {
    for (name, text) in [("ours", ours), ("base", base), ("theirs", theirs)] {
        fs::write(root.join(name), text).unwrap();
    }
    let output = Command::new("git")
        .args(["merge-file", "--diff3", "-p"])
        .arg(root.join("ours"))
        .arg(root.join("base"))
        .arg(root.join("theirs"))
        .output()
        .unwrap();
    // git merge-file returns the number of conflicts, capped at 127.
    assert!(
        output
            .status
            .code()
            .is_some_and(|code| (1..=127).contains(&code)),
        "fixture must contain a real text conflict: {:?}",
        output.status
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
#[ignore = "requires PNPM_BIN, Git and registry access"]
fn independent_changes_match_pnpm_conflict_repair_and_frozen_install() {
    let root = tempfile::tempdir().unwrap();
    let conflict = git_conflict(root.path(), OURS, BASE, THEIRS);
    let base = Lockfile::parse(BASE).unwrap();
    let ours = Lockfile::parse(OURS).unwrap();
    let theirs = Lockfile::parse(THEIRS).unwrap();
    let start = Instant::now();
    let result = locksmith::merge(&base, &ours, &theirs).unwrap();
    let rendered = result.to_yaml().unwrap();
    let merge_time = start.elapsed();
    let native = root.path().join("native");
    let pnpm = root.path().join("pnpm");
    let store = root.path().join("store");
    workspace(&native, &rendered);
    workspace(&pnpm, &conflict);
    let start = Instant::now();
    install(&pnpm, &store, false);
    let install_time = start.elapsed();
    install(&native, &store, true);
    assert_eq!(
        fs::read_to_string(native.join("pnpm-lock.yaml")).unwrap(),
        rendered
    );
    let repaired =
        Lockfile::parse(&fs::read_to_string(pnpm.join("pnpm-lock.yaml")).unwrap()).unwrap();
    repaired.validate().unwrap();
    for section in ["importers", "packages", "snapshots"] {
        assert_eq!(
            result.project().unwrap()[section],
            repaired.project().unwrap()[section],
            "{section}"
        );
    }
    eprintln!(
        "library merge+render {:?}; pnpm conflict repair+install {:?}; graphs equal",
        merge_time, install_time
    );
}

#[test]
#[ignore = "requires PNPM_BIN, Git and registry access"]
fn competing_pins_require_resolution_and_pnpm_uses_the_merged_manifest() {
    let mut ours = Lockfile::parse(OURS).unwrap().project().unwrap().clone();
    ours["importers"]["."]["dependencies"]["is-buffer"] =
        json!({"specifier":"1.1.6","version":"1.1.6"});
    let ours_text = serde_saphyr::to_string(&ours).unwrap();
    let result = locksmith::merge(
        &Lockfile::parse(BASE).unwrap(),
        &Lockfile::parse(&ours_text).unwrap(),
        &Lockfile::parse(THEIRS).unwrap(),
    );
    assert!(matches!(result, Err(LockfileError::Conflicts(_))));
    let root = tempfile::tempdir().unwrap();
    let conflict = git_conflict(root.path(), &ours_text, BASE, THEIRS);
    let pnpm = root.path().join("pnpm");
    workspace(&pnpm, &conflict);
    install(&pnpm, &root.path().join("store"), false);
    let repaired =
        Lockfile::parse(&fs::read_to_string(pnpm.join("pnpm-lock.yaml")).unwrap()).unwrap();
    assert_eq!(
        repaired.project().unwrap()["importers"]["."]["dependencies"]["is-buffer"]["version"],
        Value::String("2.0.5".into())
    );
    install(&pnpm, &root.path().join("store"), true);
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn manifest_driven_convergence_installs_and_removes_unrelated_workspace_drift() {
    use locksmith::{ManifestChange, converge};
    let mut baseline = Lockfile::parse(include_str!("fixtures/pnpm10/base.yaml"))
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
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["."]["dependencies"]["is-number"]["specifier"],
        "7.0.0"
    );
    fs::write(dir.join("pnpm-lock.yaml"), output.to_yaml().unwrap()).unwrap();
    install(&dir, &root.path().join("store"), true);
}
