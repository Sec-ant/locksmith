use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use locksmith::Lockfile;
use serde_json::{Value, json};
use tempfile::TempDir;

const CLI: &str = env!("CARGO_BIN_EXE_locksmith");

struct Repo(TempDir);
impl Repo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@example.org"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.git(&["config", "rerere.enabled", "false"]);
        repo.git(&[
            "config",
            "merge.locksmith.driver",
            &format!(
                "'{}' merge --git-driver %O %A %B %P",
                CLI.replace('\'', "'\\''")
            ),
        ]);
        repo.write(".gitattributes", "/pnpm-lock.yaml merge=locksmith\n");
        repo.write(
            "pnpm-workspace.yaml",
            "packages:\n  - 'apps/*'\n  - '!apps/ignored'\n",
        );
        repo.write(
            "package.json",
            "{\"private\":true,\"packageManager\":\"pnpm@10.34.9\"}",
        );
        repo.side("base");
        repo.commit("base");
        repo.git(&["branch", "topic"]);
        repo
    }

    fn cmd(&self, exe: &str, args: &[&str]) -> Output {
        Command::new(exe)
            .current_dir(self.0.path())
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }
    fn git(&self, args: &[&str]) -> String {
        let out = self.cmd("git", args);
        assert!(out.status.success(), "{args:?}: {}", report(&out));
        String::from_utf8(out.stdout).unwrap()
    }
    fn write(&self, path: &str, value: &str) {
        let path = self.0.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }
    fn commit(&self, name: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", name]);
    }
    fn side(&self, side: &str) {
        let (left, right) = (
            if side == "ours" { "2.0.0" } else { "1.0.0" },
            if side == "theirs" { "3.0.0" } else { "1.0.0" },
        );
        // Distinct hunks: Git can combine both manifest edits before writing them to disk.
        self.write(
            "apps/web/package.json",
            &format!(
                r#"{{
  "name": "web",
  "dependencies": {{
    "left": "{left}"
  }},
  "devDependencies": {{
    "right": "^{right}"
  }}
}}
"#
            ),
        );
        let lock = json!({"lockfileVersion":"9.0", "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers":{".":{},"apps/web":{"dependencies":{"left":{"specifier":left,"version":left}},"devDependencies":{"right":{"specifier":format!("^{right}"),"version":right}}}},
            "packages":{format!("left@{left}"):{"resolution":{"integrity":"sha512-left"}},format!("right@{right}"):{"resolution":{"integrity":"sha512-right"}}},
            "snapshots":{format!("left@{left}"): {},format!("right@{right}"): {}}});
        self.write("pnpm-lock.yaml", &lock.to_string());
    }
    fn branches(&self, extra: impl Fn(&Self, &str)) {
        for (branch, side) in [("main", "ours"), ("topic", "theirs")] {
            self.git(&["checkout", branch]);
            self.side(side);
            extra(self, side);
            self.commit(side);
        }
        self.git(&["checkout", "main"]);
    }
}

fn report(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
fn graph(data: &[u8]) -> Value {
    let lock = Lockfile::parse(std::str::from_utf8(data).unwrap()).unwrap();
    lock.validate().unwrap();
    lock.project().unwrap().clone()
}
fn assert_both(graph: Value) {
    let deps = &graph["importers"]["apps/web"]["dependencies"];
    assert_eq!(deps["left"]["version"], "2.0.0");
    assert_eq!(
        graph["importers"]["apps/web"]["devDependencies"]["right"]["version"],
        "3.0.0"
    );
}
#[test]
fn normal_merge_collects_the_cleanly_merged_manifest() {
    let r = Repo::new();
    r.branches(|_, _| {});
    let merge = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(merge.status.success(), "{}", report(&merge));
    assert_both(graph(r.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes()));
}
#[test]
fn unrelated_source_and_excluded_manifest_conflicts_leave_lock_resolved() {
    let r = Repo::new();
    r.branches(|r, side| {
        r.write("index.ts", side);
        r.write("apps/ignored/package.json", side);
    });
    let merge = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(!merge.status.success());
    assert_eq!(
        r.git(&["diff", "--name-only", "--diff-filter=U"]),
        "apps/ignored/package.json\nindex.ts\n",
        "{}",
        report(&merge)
    );
    assert_both(graph(r.git(&["show", ":pnpm-lock.yaml"]).as_bytes()));
}
#[test]
fn names_conflicted_configuration_and_preserves_all_lock_stages() {
    let r = Repo::new();
    r.branches(|r, side| {
        let path = r.0.path().join("apps/web/package.json");
        let contents = fs::read_to_string(path)
            .unwrap()
            .replace("\"web\"", &format!("\"{side}\""));
        r.write("apps/web/package.json", &contents);
        r.write(
            ".npmrc",
            &format!("strict-peer-dependencies={}\n", side == "ours"),
        );
    });
    let merge = r.cmd("git", &["merge", "--no-edit", "topic"]);
    let message = report(&merge);
    assert!(!merge.status.success());
    assert!(
        message
            .contains("resolve dependency configuration first:\n  .npmrc\n  apps/web/package.json"),
        "{message}"
    );
    assert_eq!(
        r.git(&["ls-files", "-u", "--", "pnpm-lock.yaml"])
            .lines()
            .count(),
        3
    );
    let text = fs::read_to_string(r.0.path().join("pnpm-lock.yaml")).unwrap();
    assert!(text.contains("<<<<<<< ours"));
    assert!(text.contains("======="));
    assert!(text.contains(">>>>>>> theirs"));
    assert!(text.contains("left@2.0.0") && text.contains("right@3.0.0"));
}
#[test]
fn ordinary_cli_merge_works_outside_git() {
    let temp = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../locksmith/tests/fixtures/pnpm10");
    let output = Command::new(CLI)
        .arg("merge")
        .args(["base.yaml", "ours.yaml", "theirs.yaml"].map(|p| fixtures.join(p)))
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", report(&output));
    let graph = graph(&output.stdout);
    let deps = &graph["importers"]["."]["dependencies"];
    assert_eq!(deps["is-even"]["version"], "1.0.0");
    assert_eq!(deps["is-buffer"]["version"], "2.0.5");
}
#[test]
fn converge_reads_ref_and_worktree_and_outputs_only_required_changes() {
    let r = Repo::new();
    let path = r.0.path().join("pnpm-lock.yaml");
    let baseline = fs::read(&path).unwrap();
    let same = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(same.status.success(), "{}", report(&same));
    assert_eq!(same.stdout, baseline);
    let workspace = r.0.path().join("pnpm-workspace.yaml");
    let yaml = fs::read_to_string(&workspace).unwrap();
    fs::write(&workspace, format!("# workspace packages\n{yaml}")).unwrap();
    let equivalent = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(equivalent.status.success(), "{}", report(&equivalent));
    assert_eq!(equivalent.stdout, baseline);
    r.side("ours");
    let mut drifted = graph(&fs::read(&path).unwrap());
    drifted["importers"]["apps/web"]["devDependencies"]["right"]["version"] = json!("1.5.0");
    for section in ["packages", "snapshots"] {
        let old = drifted[section]
            .as_object_mut()
            .unwrap()
            .remove("right@1.0.0")
            .unwrap();
        drifted[section]["right@1.5.0"] = old;
    }
    r.write("pnpm-lock.yaml", &drifted.to_string());
    let candidate = fs::read(&path).unwrap();
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(
        graph(&out.stdout)["importers"]["apps/web"]["dependencies"]["left"]["version"],
        "2.0.0"
    );
    assert_eq!(
        graph(&out.stdout)["importers"]["apps/web"]["devDependencies"]["right"]["version"],
        "1.0.0"
    );
    assert_eq!(fs::read(&path).unwrap(), candidate);
    r.write("pnpm-workspace.yaml", "packages: ['other/*']");
    let rejected = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(!rejected.status.success());
    assert!(report(&rejected).contains("pnpm-workspace.yaml"));
    assert!(rejected.stdout.is_empty());
}

#[test]
fn a_widened_catalog_keeps_one_release_across_new_and_existing_consumers() {
    let r = Repo::new();
    for (install, range, version) in [(false, "1.0.0", "1.0.0"), (true, "^1", "1.1.0")] {
        r.write(
            "pnpm-workspace.yaml",
            &format!("packages: ['apps/*']\ncatalog:\n  right: '{range}'\n"),
        );
        let mut doc = json!({"lockfileVersion":"9.0","importers":{".":{}},
            "catalogs":{"default":{"right":{"specifier":range,"version":version}}},
            "packages":{format!("right@{version}"):{"resolution":{"integrity":"sha512-right"}}},
            "snapshots":{format!("right@{version}"):{}}});
        let group = if install {
            "devDependencies"
        } else {
            "dependencies"
        };
        let consumers = if install {
            &["web", "mobile"][..]
        } else {
            &["web"][..]
        };
        for id in consumers {
            r.write(
                &format!("apps/{id}/package.json"),
                &json!({"name":id,group:{"right":"catalog:"}}).to_string(),
            );
            doc["importers"][format!("apps/{id}")] =
                json!({group:{"right":{"specifier":"catalog:","version":version}}});
        }
        r.write("pnpm-lock.yaml", &doc.to_string());
        if !install {
            r.commit("catalog baseline");
        }
    }
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    let doc = graph(&out.stdout);
    assert_eq!(
        doc["catalogs"]["default"]["right"],
        json!({"specifier":"^1","version":"1.0.0"})
    );
    for id in ["apps/web", "apps/mobile"] {
        assert_eq!(
            doc["importers"][id],
            json!({"devDependencies":{"right":{"specifier":"catalog:","version":"1.0.0"}}})
        );
    }
    assert_eq!(doc["snapshots"], json!({"right@1.0.0":{}}));
    fs::write(r.0.path().join("pnpm-lock.yaml"), &out.stdout).unwrap();
    let repeated = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(repeated.status.success(), "{}", report(&repeated));
    assert_eq!(repeated.stdout, out.stdout);
}

#[test]
fn merged_intent_with_an_unavailable_release_leaves_lock_unmerged() {
    let r = Repo::new();
    r.branches(|r, side| {
        if side == "ours" {
            let path = r.0.path().join("apps/web/package.json");
            let manifest = fs::read_to_string(path).unwrap().replace("2.0.0", "11.3.2");
            r.write("apps/web/package.json", &manifest);
        }
    });
    let merge = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(!merge.status.success());
    assert_eq!(
        r.git(&["diff", "--name-only", "--diff-filter=U"]),
        "pnpm-lock.yaml\n"
    );
    assert!(report(&merge).contains("left"), "{}", report(&merge));
}

#[test]
fn conflicted_patch_is_named_even_when_the_manifests_merge_cleanly() {
    let r = Repo::new();
    r.branches(|r, side| {
        r.write("pnpm-workspace.yaml", "packages: ['apps/*', '!apps/ignored']\npatchedDependencies:\n  left: ./patches/left.patch\n");
        r.write("patches/left.patch", side);
    });
    let out = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(!out.status.success());
    assert!(
        report(&out).contains("resolve dependency configuration first:\n  patches/left.patch"),
        "{}",
        report(&out)
    );
    assert_eq!(
        r.git(&["ls-files", "-u", "--", "pnpm-lock.yaml"])
            .lines()
            .count(),
        3
    );
}

#[test]
fn explicit_cli_context_selects_pins_without_a_git_checkout() {
    let r = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    for side in ["base", "ours", "theirs"] {
        r.side(side);
        fs::copy(r.0.path().join("pnpm-lock.yaml"), dir.path().join(side)).unwrap();
    }
    let context = json!({
        "target":"ours",
        "manifests": {".":{}, "apps/web":{"dependencies":{"left":"2.0.0"},"devDependencies":{"right":"^3.0.0"}}},
        "configuration":{}, "configurations":[{},{},{}], "patches":{}
    });
    fs::write(dir.path().join("context.json"), context.to_string()).unwrap();
    let out = Command::new(CLI)
        .current_dir(dir.path())
        .args([
            "merge",
            "base",
            "ours",
            "theirs",
            "--context",
            "context.json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", report(&out));
    assert_both(graph(&out.stdout));
}

#[test]
fn help_version_and_usage_errors_work_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(CLI)
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap()
    };
    for name in ["merge", "converge", "repair"] {
        let out = run(&[name, "--help"]);
        assert!(out.status.success());
        let alias = run(&["help", name]);
        assert!(alias.status.success());
        assert_eq!(alias.stdout, out.stdout);
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .contains(&format!("locksmith {name} "))
        );
        assert!(out.stderr.is_empty());
    }
    let version = run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!("locksmith {}", env!("CARGO_PKG_VERSION"))
    );
    let invalid = run(&["merge"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(
        String::from_utf8(invalid.stderr)
            .unwrap()
            .contains("merge BASE OURS THEIRS")
    );
    for args in [
        &["merge", "base", "ours", "theirs", "--allow-network"][..],
        &[
            "merge",
            "--git-driver",
            "base",
            "ours",
            "theirs",
            "pnpm-lock.yaml",
            "--context",
            "context.json",
        ],
    ] {
        let invalid = run(args);
        assert_eq!(invalid.status.code(), Some(2));
        assert!(invalid.stdout.is_empty());
        assert!(!invalid.stderr.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn ordinary_merge_accepts_non_utf8_file_paths() {
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .join(std::ffi::OsStr::from_bytes(b"lock-\xff.yaml"));
    let lock = b"lockfileVersion: '9.0'\nimporters: {'.': {}}\n";
    fs::write(&path, lock).unwrap();
    let out = Command::new(CLI)
        .arg("merge")
        .args([&path; 3])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, lock);
}

fn pin_right(repo: &Repo, version: &str) {
    let mut lock = graph(&fs::read(repo.0.path().join("pnpm-lock.yaml")).unwrap());
    let pin = &mut lock["importers"]["apps/web"]["devDependencies"]["right"]["version"];
    let previous = format!("right@{}", pin.as_str().unwrap());
    *pin = json!(version);
    for section in ["packages", "snapshots"] {
        let records = lock[section].as_object_mut().unwrap();
        let record = records.remove(&previous).unwrap();
        records.insert(format!("right@{version}"), record);
    }
    repo.write("pnpm-lock.yaml", &lock.to_string());
}

#[test]
fn driver_prefers_default_branch_pins_in_either_merge_direction() {
    let r = Repo::new();
    r.git(&["branch", "-m", "trunk"]);
    pin_right(&r, "1.1.0");
    r.commit("trunk pin");
    r.git(&["update-ref", "refs/remotes/origin/trunk", "HEAD"]);
    r.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
    ]);
    r.git(&["switch", "topic"]);
    pin_right(&r, "1.2.0");
    r.commit("topic pin");
    for (branch, incoming) in [("topic", "trunk"), ("trunk", "topic")] {
        r.git(&["switch", branch]);
        r.git(&["merge", "--no-commit", incoming]);
        let lock = graph(r.git(&["show", ":pnpm-lock.yaml"]).as_bytes());
        assert_eq!(
            lock["importers"]["apps/web"]["devDependencies"]["right"]["version"],
            "1.1.0"
        );
        r.git(&["merge", "--abort"]);
    }

    r.git(&[
        "config",
        "--local",
        "locksmith.mainline",
        "refs/heads/topic",
    ]);
    r.git(&["switch", "trunk"]);
    r.git(&["merge", "--no-commit", "topic"]);
    let lock = graph(r.git(&["show", ":pnpm-lock.yaml"]).as_bytes());
    assert_eq!(
        lock["importers"]["apps/web"]["devDependencies"]["right"]["version"],
        "1.2.0"
    );
    r.git(&["merge", "--abort"]);

    r.git(&[
        "config",
        "--local",
        "locksmith.mainline",
        "refs/heads/missing",
    ]);
    let result = r.cmd("git", &["merge", "--no-commit", "topic"]);
    assert!(!result.status.success());
    assert!(report(&result).contains("invalid locksmith.mainline \"refs/heads/missing\""));
}

#[test]
fn driver_prefers_ours_without_a_mainline() {
    let r = Repo::new();
    pin_right(&r, "1.1.0");
    r.commit("main pin");
    r.git(&["branch", "master"]);
    r.git(&["switch", "topic"]);
    pin_right(&r, "1.2.0");
    r.commit("topic pin");

    r.git(&["merge", "--no-commit", "main"]);
    let lock = graph(r.git(&["show", ":pnpm-lock.yaml"]).as_bytes());
    assert_eq!(
        lock["importers"]["apps/web"]["devDependencies"]["right"]["version"],
        "1.2.0"
    );
}

#[test]
fn rebase_replays_each_commit_against_its_own_parent() {
    let r = Repo::new();
    r.side("ours");
    pin_right(&r, "1.1.0");
    r.commit("main changes left and pins right");
    r.git(&["switch", "topic"]);
    pin_right(&r, "1.2.0");
    r.write("one", "keep first commit");
    r.commit("compatible pin");
    r.side("theirs");
    r.commit("require right 3");
    r.git(&["rebase", "main"]);
    let first = graph(r.git(&["show", "HEAD^:pnpm-lock.yaml"]).as_bytes());
    assert_eq!(
        first["importers"]["apps/web"]["devDependencies"]["right"]["version"],
        "1.1.0"
    );
    assert_both(graph(r.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes()));
}

#[test]
fn rebase_reports_conflicted_manifest_and_retains_lock_stages() {
    let r = Repo::new();
    r.branches(|r, side| {
        let manifest = fs::read_to_string(r.0.path().join("apps/web/package.json")).unwrap();
        r.write(
            "apps/web/package.json",
            &manifest.replace("\"web\"", &format!("\"{side}\"")),
        );
    });
    r.git(&["switch", "topic"]);
    let result = r.cmd("git", &["rebase", "main"]);
    assert!(!result.status.success());
    assert!(report(&result).contains("apps/web/package.json"));
    assert_eq!(
        r.git(&["ls-files", "-u", "--", "pnpm-lock.yaml"])
            .lines()
            .count(),
        3
    );
}

#[test]
fn clean_text_fallback_still_requires_manual_acceptance() {
    let r = Repo::new();
    let path = "pnpm-lock.yaml";
    let original = r.git(&["show", "HEAD:pnpm-lock.yaml"]);
    let baseline = format!("# header\n\n{original}\n\n# footer\n");
    r.write(path, &baseline);
    r.commit("separate header and footer");
    r.git(&["branch", "-f", "topic", "HEAD"]);
    r.branches(|r, side| {
        let content = match side {
            "ours" => baseline.replace("# header", "# local header"),
            _ => baseline.replace("# footer", "# incoming footer"),
        };
        r.write(path, &content);
    });
    let result = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(!result.status.success());
    assert!(report(&result).contains("text merged cleanly"));
    let expected = baseline
        .replace("# header", "# local header")
        .replace("# footer", "# incoming footer");
    assert_eq!(fs::read_to_string(r.0.path().join(path)).unwrap(), expected);
    assert_eq!(r.git(&["ls-files", "-u", "--", path]).lines().count(), 3);
}

#[test]
fn repair_uses_the_selected_commit_and_recovers_a_missing_parent_record() {
    let repo = Repo::new();
    let original = repo.git(&["show", "HEAD:pnpm-lock.yaml"]);
    let mut damaged: Value = serde_json::from_str(&original).unwrap();
    damaged["snapshots"]
        .as_object_mut()
        .unwrap()
        .remove("left@1.0.0")
        .unwrap();
    repo.write("pnpm-lock.yaml", &damaged.to_string());
    repo.commit("drop one live snapshot");
    repo.write("package.json", "{unfinished");
    repo.write("pnpm-lock.yaml", "working tree edits");
    let out = repo.cmd(CLI, &["repair"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(graph(&out.stdout), graph(original.as_bytes()));
    assert_eq!(
        fs::read(repo.0.path().join("pnpm-lock.yaml")).unwrap(),
        b"working tree edits"
    );
    let out = repo.cmd(CLI, &["repair", "HEAD^"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, original.as_bytes());
}

#[test]
fn repair_reports_ambiguous_duplicates_without_emitting_yaml() {
    let repo = Repo::new();
    repo.write("pnpm-lock.yaml", "lockfileVersion: '9.0'\nimporters: {}\nsnapshots:\n  x@1.0.0: {}\n  x@1.0.0: {dependencies: {y: 2.0.0}}\n");
    repo.commit("ambiguous duplicate");
    let out = repo.cmd(CLI, &["repair"]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("x@1.0.0"));
}
