//! Real package-manager differential tests (network and Git required).
//! `PNPM_BIN=/absolute/path/to/pnpm cargo test --test pnpm -- --ignored --nocapture`
use locksmith::WorkspaceConfig;
use serde_json::Value;
use std::{
    env, fs,
    path::Path,
    process::{Command, Output},
};

const BASE: &str = "lockfileVersion: '9.0'\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\nimporters:\n  .: {}\n";
const OURS: &str = include_str!("../fixtures/pnpm10/ours.yaml");
const THEIRS: &str = include_str!("../fixtures/pnpm10/theirs.yaml");

mod configuration;
mod converge;
mod merge;
mod peers;

fn check_frozen(directory: &Path, store: &Path) -> Output {
    Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/check-frozen.sh"
        ))
        .args([
            "--config.manage-package-manager-versions=false",
            "--store-dir",
        ])
        .arg(store)
        .current_dir(directory)
        .output()
        .unwrap()
}

// Some pnpm releases write installation state even with enable-modules-dir=false.
// Keep the probe independent of the install whose files the tests inspect.
fn copy_project(from: &Path, to: &Path, store: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "node_modules"
            || entry.file_name() == ".git"
            || entry.path() == store
        {
            continue;
        }
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir(&dest).unwrap();
            copy_project(&entry.path(), &dest, store);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn install(directory: &Path, store: &Path, frozen: bool) {
    let before = frozen.then(|| fs::read(directory.join("pnpm-lock.yaml")).unwrap());
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
    assert_success(command.output().unwrap());
    if let Some(before) = before {
        assert!(
            fs::read(directory.join("pnpm-lock.yaml")).unwrap() == before,
            "frozen install changed pnpm-lock.yaml in {}",
            directory.display()
        );
        let probe = tempfile::tempdir().unwrap();
        copy_project(directory, probe.path(), store);
        assert_success(check_frozen(probe.path(), store));
    }
}

fn cfg(value: &Value) -> WorkspaceConfig {
    WorkspaceConfig::from_pnpm_version(value, pnpm_version()).unwrap()
}

fn override_tie_break() -> locksmith::OverrideTieBreak {
    if pnpm_version().major >= 12 {
        locksmith::OverrideTieBreak::First
    } else {
        locksmith::OverrideTieBreak::Last
    }
}

fn pnpm_version() -> &'static node_semver::Version {
    static VERSION: std::sync::OnceLock<node_semver::Version> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        let output = Command::new(env::var_os("PNPM_BIN").unwrap())
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        let version = String::from_utf8(output.stdout).unwrap();
        version.trim().parse().unwrap()
    })
}

#[test]
#[ignore = "requires PNPM_BIN and registry access"]
fn offline_probe_detects_a_missing_transitive_snapshot_after_a_successful_install() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let store = root.join("store");
    fs::write(
        root.join("package.json"),
        r#"{"name":"offline-check","dependencies":{"is-odd":"3.0.1"}}"#,
    )
    .unwrap();
    install(root, &store, false);
    assert_success(check_frozen(root, &store));
    let path = root.join("pnpm-lock.yaml");
    let before = fs::read_to_string(&path).unwrap();
    let (head, snapshots) = before.rsplit_once("\nsnapshots:").unwrap();
    let key = "  is-number@6.0.0: {}";
    assert!(snapshots.contains(key));
    let broken = format!("{head}\nsnapshots:{}", snapshots.replace(key, ""));
    fs::write(&path, &broken).unwrap();
    let output = check_frozen(root, &store);
    assert!(
        !output.status.success(),
        "missing transitive snapshot passed the offline check"
    );
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("is-number@6.0.0"), "{message}");
}

#[cfg(unix)]
#[test]
fn a_successful_probe_that_rewrites_one_byte_fails_acceptance() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let pnpm = dir.path().join("pnpm");
    fs::write(&pnpm, "#!/bin/sh\nprintf '\\n' >> pnpm-lock.yaml\n").unwrap();
    fs::set_permissions(&pnpm, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.path().join("pnpm-lock.yaml"), BASE).unwrap();
    let out = Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/check-frozen.sh"
        ))
        .env("PNPM_BIN", &pnpm)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("changed pnpm-lock.yaml"));
}
