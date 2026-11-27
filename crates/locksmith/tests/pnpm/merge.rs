use super::{BASE, OURS, THEIRS, cfg, install};
use locksmith::{Lockfile, LockfileError};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

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
    let result = locksmith::merge(&base, &ours, &theirs, None).unwrap();
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
        None,
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
fn duplicate_snapshots_converge_to_the_installed_peer_graph() {
    use locksmith::{MergeContext, MergeTarget, merge};
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    let before = json!({"packages":[],"packageExtensions":{"is-number@6.0.0":{"dependencies":{"react-dom":"18.2.0"}}},"overrides":{"is-number>react-dom":"18.3.1"}});
    let mut after = before.clone();
    after["overrides"]["is-number>react-dom"] = json!("18.2.0");
    let prepare = |name: &str, config: &Value, donor: bool| {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        let deps = if donor {
            json!({"react-dom":"18.2.0","react":"18.3.1"})
        } else {
            json!({"is-number":"6.0.0","react":"18.3.1"})
        };
        let manifest = json!({"name":"duplicate-snapshot-test","private":true,"dependencies":deps,"pnpm":config});
        fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            serde_saphyr::to_string(config).unwrap(),
        )
        .unwrap();
        (dir, manifest)
    };
    let (ours, _) = prepare("ours", &before, false);
    let (donor, _) = prepare("donor", &before, true);
    let (native, _) = prepare("native", &after, false);
    let texts = [&ours, &donor, &native].map(|dir| {
        install(dir, &store, false);
        fs::read_to_string(dir.join("pnpm-lock.yaml")).unwrap()
    });
    let locks = texts.each_ref().map(|text| Lockfile::parse(text).unwrap());
    let mut document = locks[0].project().unwrap().clone();
    let snapshots = document
        .as_object_mut()
        .unwrap()
        .remove("snapshots")
        .unwrap();
    let (key, body) = snapshots
        .as_object()
        .unwrap()
        .iter()
        .find(|(key, _)| key.split('(').next() == Some("is-number@6.0.0"))
        .unwrap();
    let mut alternative = body.clone();
    alternative["dependencies"]["react-dom"] =
        locks[1].project().unwrap()["importers"]["."]["dependencies"]["react-dom"]["version"]
            .clone();
    assert_ne!(&alternative, body);
    let mut duplicated = serde_saphyr::to_string(&document).unwrap();
    duplicated.push_str("snapshots:\n");
    for map in [snapshots.clone(), json!({key: alternative})] {
        for line in serde_saphyr::to_string(&map).unwrap().lines() {
            duplicated.push_str("  ");
            duplicated.push_str(line);
            duplicated.push('\n');
        }
    }
    let (dir, manifest) = prepare("merged", &after, false);
    let old = cfg(&before);
    let final_config = cfg(&after);
    let result = merge(
        texts[0].as_str(),
        duplicated.as_str(),
        texts[1].as_str(),
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &BTreeMap::from([(".".into(), manifest)]),
            configurations: [&old; 3],
            configuration: &final_config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    for section in ["importers", "packages", "snapshots"] {
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
