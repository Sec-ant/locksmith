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
