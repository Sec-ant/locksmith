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
            "packages:\n  - 'apps/*'\n  - '!apps/ignored'\npmOnFail: ignore\n",
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
        write_input(self.0.path(), path, value);
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
fn git_merge_reads_the_final_workspace_with_the_selected_bootstrap_version() {
    let repo = Repo::new();
    repo.write(
        "package.json",
        r#"{"devEngines":{"packageManager":{"name":"pnpm","version":"^12.18.0"}}}"#,
    );
    let write_project = |pnpm: &str, tool: &str, scoped: bool| {
        let settings = if scoped {
            "projectScopedOptionalPeers: true\n"
        } else {
            ""
        };
        repo.write(
            "pnpm-workspace.yaml",
            &format!("packages: []\nconfigDependencies: {{tool: '{tool}'}}\n{settings}"),
        );
        let bootstrap = json!({"lockfileVersion":"9.0","importers":{".":{
            "configDependencies":{"tool":{"specifier":tool,"version":tool}},
            "packageManagerDependencies":{"pnpm":{"specifier":"^12.18.0","version":pnpm}}}},
            "packages":{format!("pnpm@{pnpm}"):{"resolution":{}},format!("tool@{tool}"):{"resolution":{}}},
            "snapshots":{format!("pnpm@{pnpm}"): {},format!("tool@{tool}"): {}}});
        let mut project = json!({"lockfileVersion":"9.0","importers":{".":{}}});
        if scoped {
            project["settings"] = json!({"projectScopedOptionalPeers":true});
        }
        repo.write(
            "pnpm-lock.yaml",
            &format!("---\n{bootstrap}\n---\n{project}\n"),
        );
    };
    write_project("12.18.0", "1.0.0", false);
    repo.commit("configure the bootstrap range");
    repo.git(&["branch", "-f", "topic", "HEAD"]);
    write_project("12.18.1", "1.0.0", false);
    repo.commit("update pnpm on main");
    repo.git(&["switch", "topic"]);
    write_project("12.21.0", "2.0.0", true);
    repo.commit("update the configuration package and pnpm");
    repo.git(&["switch", "main"]);
    repo.git(&["merge", "--no-commit", "topic"]);
    let merged = Lockfile::parse(&repo.git(&["show", ":pnpm-lock.yaml"])).unwrap();
    assert_eq!(
        merged.recorded_pnpm_version().unwrap().to_string(),
        "12.21.0"
    );
    assert_eq!(
        merged.project().unwrap()["settings"]["projectScopedOptionalPeers"],
        true
    );
}

#[test]
fn git_merge_keeps_the_bootstrap_of_the_runner_that_loaded_the_workspace() {
    let repo = Repo::new();
    repo.write("package.json", "{}");
    repo.write("pnpm-workspace.yaml", "packages: []\n");
    let project = "lockfileVersion: '9.0'\nimporters: {.: {}}\n";
    repo.write("pnpm-lock.yaml", project);
    repo.commit("project without a bootstrap manager");
    repo.git(&["branch", "-f", "topic", "HEAD"]);
    repo.write("pnpm-lock.yaml", &format!("{project}# main\n"));
    repo.commit("change the project lock on main");
    repo.git(&["switch", "topic"]);
    repo.write("package.json", r#"{"pnpm":false}"#);
    let env = json!({"lockfileVersion":"9.0","importers":{".":{"packageManagerDependencies":{
        "pnpm":{"specifier":"^12","version":"12.21.0"}}}},
        "packages":{"pnpm@12.21.0":{"resolution":{}}},"snapshots":{"pnpm@12.21.0":{}}});
    repo.write("pnpm-lock.yaml", &format!("---\n{env}\n---\n{project}"));
    repo.commit("upgrade to a runner that ignores the legacy pnpm field");
    repo.git(&["switch", "main"]);
    repo.git(&["merge", "--no-commit", "topic"]);
    let lock = Lockfile::parse(&repo.git(&["show", ":pnpm-lock.yaml"])).unwrap();
    assert_eq!(
        lock.recorded_pnpm_version(),
        Some("12.21.0".parse().unwrap())
    );
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
fn only_active_hook_conflicts_block_the_lockfile_merge() {
    for ignored in [true, false] {
        let repo = Repo::new();
        repo.write(
            "pnpm-workspace.yaml",
            &format!(
                "packages: ['apps/*']\nignorePnpmfile: {ignored}\npnpmfile: hooks/custom.cjs\n"
            ),
        );
        repo.commit("configure the workspace hook");
        repo.git(&["branch", "-f", "topic", "HEAD"]);
        repo.branches(|repo, side| {
            for path in [".pnpmfile.cjs", "hooks/custom.cjs"] {
                repo.write(path, side);
            }
        });

        let out = repo.cmd("git", &["merge", "--no-edit", "topic"]);
        assert!(!out.status.success());
        let conflicts = repo.git(&["diff", "--name-only", "--diff-filter=U"]);
        if ignored {
            assert_eq!(
                conflicts,
                ".pnpmfile.cjs\nhooks/custom.cjs\n",
                "{}",
                report(&out)
            );
            assert_both(graph(repo.git(&["show", ":pnpm-lock.yaml"]).as_bytes()));
        } else {
            assert_eq!(
                conflicts,
                ".pnpmfile.cjs\nhooks/custom.cjs\npnpm-lock.yaml\n",
                "{}",
                report(&out)
            );
            assert!(report(&out).contains("resolve dependency configuration first"));
        }
    }
}

#[cfg(unix)]
#[test]
fn only_the_selected_default_hook_follows_conflicted_symlink_targets() {
    for (hook, active) in [(".pnpmfile.cjs", true), (".pnpmfile.mjs", false)] {
        let repo = Repo::new();
        repo.write("tools/hook", "module.exports = {};\n");
        std::os::unix::fs::symlink("tools/hook", repo.0.path().join(hook)).unwrap();
        repo.commit("store the hook through a symlink");
        repo.git(&["branch", "-f", "topic", "HEAD"]);
        repo.branches(|r, side| r.write("tools/hook", side));
        let out = repo.cmd("git", &["merge", "--no-edit", "topic"]);
        assert!(!out.status.success());
        let conflicts = repo.git(&["diff", "--name-only", "--diff-filter=U"]);
        assert!(conflicts.lines().any(|p| p == "tools/hook"));
        assert_eq!(
            conflicts.lines().any(|p| p == "pnpm-lock.yaml"),
            active,
            "{hook}: {}",
            report(&out)
        );
    }
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
fn converge_in_a_nested_workspace_discovers_untracked_members() {
    for (pattern, member) in [("apps/*", "apps/web"), ("apps/**", "apps/team/web")] {
        let r = Repo::new();
        r.write("nested/package.json", "{}");
        r.write(
            "nested/pnpm-workspace.yaml",
            &format!("packages: ['{pattern}']"),
        );
        r.write("nested/.gitignore", "apps/ignored/\n");
        let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{}}});
        r.write("nested/pnpm-lock.yaml", &lock.to_string());
        r.commit("nested workspace");

        r.write("nested/apps/ignored/package.json", "ignored by Git");
        r.write(
            &format!("nested/{member}/package.json"),
            r#"{"name":"new-member","dependencies":{"local":"link:../local"}}"#,
        );
        lock["importers"][member] = json!({"dependencies":{"local":{"specifier":"link:../local","version":"link:../local"}}});
        r.write("nested/pnpm-lock.yaml", &lock.to_string());

        let out = Command::new(CLI)
            .current_dir(r.0.path().join("nested"))
            .args(["converge", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", report(&out));
        assert_eq!(graph(&out.stdout)["importers"], lock["importers"]);
    }
}

#[test]
fn converge_across_a_pnpm_upgrade_keeps_baseline_pins_and_applies_final_overrides() {
    let r = Repo::new();
    let make = |child: &str, old: bool| {
        let mut lock = json!({"lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers":{".":{"dependencies":{"plugin":{"specifier":"1.0.0","version":"1.0.0"}}}},
            "packages":{"plugin@1.0.0":{"resolution":{"integrity":"sha512-plugin"}},
                format!("child@{child}"):{"resolution":{"integrity":format!("sha512-{child}")}}},
            "snapshots":{"plugin@1.0.0":{"dependencies":{"child":child}},format!("child@{child}"):{}}});
        if old {
            lock["packages"]["plugin@1.0.0"]["peerDependencies"] = json!({"color":"*"});
            lock["packages"]["plugin@1.0.0"]["peerDependenciesMeta"] =
                json!({"color":{"optional":true}});
        } else {
            lock["overrides"] = json!({"plugin>color":"-"});
        }
        lock.to_string()
    };
    let manifest = |version: &str| {
        json!({"packageManager":format!("pnpm@{version}"),"dependencies":{"plugin":"1.0.0"}})
            .to_string()
    };
    r.git(&["rm", "apps/web/package.json"]);
    r.write(
        "pnpm-workspace.yaml",
        "packages: []\nonlyBuiltDependencies: []\n",
    );
    r.write("package.json", &manifest("10.33.2"));
    r.write("pnpm-lock.yaml", &make("1.0.0", true));
    r.commit("before upgrade");
    r.write(
        "pnpm-workspace.yaml",
        "packages: []\npmOnFail: ignore\nallowBuilds: {}\noverrides: {'plugin>color': '-'}\n",
    );
    r.write("package.json", &manifest("12.21.0"));
    r.write("pnpm-lock.yaml", &make("1.1.0", false));
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    let result = graph(&out.stdout);
    assert_eq!(
        result["snapshots"]["plugin@1.0.0"]["dependencies"]["child"],
        "1.0.0"
    );
    assert!(
        result["packages"]["plugin@1.0.0"]
            .get("peerDependencies")
            .is_none()
    );
    assert!(
        result["packages"]["plugin@1.0.0"]
            .get("peerDependenciesMeta")
            .is_none()
    );
    assert_eq!(result["overrides"]["plugin>color"], "-");
    r.write("pnpm-lock.yaml", std::str::from_utf8(&out.stdout).unwrap());
    let repeat = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(repeat.status.success(), "{}", report(&repeat));
    assert_eq!(repeat.stdout, out.stdout);
    r.write("pnpm-lock.yaml", &make("1.0.0", true));
    let stale = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(!stale.status.success());
    assert!(stale.stdout.is_empty());
    assert!(report(&stale).contains("overrides"));
}

#[test]
fn a_runner_upgrade_can_select_another_unchanged_default_hook() {
    let repo = Repo::new();
    repo.write(".pnpmfile.cjs", "module.exports = {};\n");
    repo.write(
        ".pnpmfile.mjs",
        "export const hooks = { readPackage(pkg) {return pkg} }\n",
    );
    let mut lock = graph(repo.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes());
    // pnpm 10 uses the CJS file; pnpm 12 selects the ES module.
    lock["pnpmfileChecksum"] = json!("sha256-giK4Fp7obyXNzNhNNANABgrj8M/1Xi6p00TXwzJzO3E=");
    repo.write("pnpm-lock.yaml", &lock.to_string());
    repo.commit("track both default hooks");
    repo.write("package.json", r#"{"packageManager":"pnpm@12.21.0"}"#);
    lock["pnpmfileChecksum"] = json!("sha256-hcrgxuD4yzd6wArCpuECMwsXbpMreZHhRY4a0MX/XXA=");
    repo.write("pnpm-lock.yaml", &lock.to_string());
    let out = repo.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(
        graph(&out.stdout)["pnpmfileChecksum"],
        lock["pnpmfileChecksum"]
    );
    repo.write(".pnpmfile.mjs", "export const hooks = {};\n");
    let out = repo.cmd(CLI, &["converge", "HEAD"]);
    assert!(!out.status.success());
}

#[test]
fn a_newly_recorded_runner_uses_the_installed_patch_contents() {
    let r = Repo::new();
    let manager = |policy| {
        json!({"devEngines":{"packageManager":{"name":"pnpm","version":"12.21.0","onFail":policy}}})
            .to_string()
    };
    r.write("package.json", &manager("ignore"));
    r.write(
        "pnpm-workspace.yaml",
        "packages: []\nallowUnusedPatches: true\npatchedDependencies: {unused: unused.patch}\n",
    );
    r.write("unused.patch", "old\n");
    let project = json!({"lockfileVersion":"9.0","importers":{".":{}},"patchedDependencies":{"unused":"01d09d19c2139a46aebfb577780d123d7396e97201bc7ead210a2ebff8239dee"}});
    r.write("pnpm-lock.yaml", &project.to_string());
    r.commit("use a patch without managing pnpm");
    r.write("package.json", &manager("download"));
    let boot = json!({"lockfileVersion":"9.0","importers":{".":{"packageManagerDependencies":{"pnpm":{"specifier":"12.21.0","version":"12.21.0"}}}},"packages":{"pnpm@12.21.0":{"resolution":{}}},"snapshots":{"pnpm@12.21.0":{}}});
    r.write("pnpm-lock.yaml", &format!("---\n{boot}\n---\n{project}\n"));
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(
        graph(&out.stdout)["patchedDependencies"],
        project["patchedDependencies"]
    );
}

#[test]
fn converge_accepts_changes_to_an_ignored_explicit_hook_across_pnpm_versions() {
    let repo = Repo::new();
    repo.write(
        "pnpm-workspace.yaml",
        "packages: ['apps/*']\npmOnFail: ignore\nignorePnpmfile: true\npnpmfile: hooks/custom.cjs\n",
    );
    repo.write("hooks/custom.cjs", "module.exports = {};\n");
    repo.commit("ignore the workspace hook");
    let original = graph(repo.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes());
    repo.write("hooks/custom.cjs", "module.exports = { hooks: {} };\n");

    for version in ["10.34.9", "12.21.0"] {
        repo.write(
            "package.json",
            &json!({"private":true,"packageManager":format!("pnpm@{version}")}).to_string(),
        );
        let out = repo.cmd(CLI, &["converge", "HEAD"]);
        assert!(out.status.success(), "{version}: {}", report(&out));
        let result = graph(&out.stdout);
        assert_eq!(result["importers"], original["importers"]);
        assert_eq!(result["snapshots"], original["snapshots"]);
    }
}

#[test]
fn converge_accepts_a_removed_dot_relative_patch_during_a_pnpm_upgrade() {
    let r = Repo::new();
    let workspace = r.git(&["show", "HEAD:pnpm-workspace.yaml"]);
    let unpatched = r.git(&["show", "HEAD:pnpm-lock.yaml"]);
    let mut baseline = graph(unpatched.as_bytes());
    // SHA-256 of the baseline patch contents, "old\n".
    let hash = "01d09d19c2139a46aebfb577780d123d7396e97201bc7ead210a2ebff8239dee";
    let pin = format!("1.0.0(patch_hash={hash})");
    baseline["patchedDependencies"] = json!({"left":{"hash":hash,"path":"patches/left.patch"}});
    baseline["importers"]["apps/web"]["dependencies"]["left"]["version"] = json!(pin);
    let snapshots = baseline["snapshots"].as_object_mut().unwrap();
    let body = snapshots.remove("left@1.0.0").unwrap();
    snapshots.insert(format!("left@{pin}"), body);
    r.write("pnpm-lock.yaml", &baseline.to_string());
    r.write("patches/left.patch", "old\n");
    r.write(
        "pnpm-workspace.yaml",
        &format!("{workspace}\npatchedDependencies:\n  left: ./patches/left.patch\n"),
    );
    r.commit("patch left");

    r.git(&["rm", "patches/left.patch"]);
    r.write("pnpm-workspace.yaml", &workspace);
    r.write("pnpm-lock.yaml", &unpatched);
    r.write(
        "package.json",
        r#"{"private":true,"packageManager":"pnpm@12.21.0"}"#,
    );
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    let result = graph(&out.stdout);
    assert!(result.get("patchedDependencies").is_none());
    assert_eq!(
        result["importers"]["apps/web"]["dependencies"]["left"]["version"],
        "1.0.0"
    );
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
fn a_pnpm_upgrade_preserves_the_new_resolvers_peers_and_mainline_additions() {
    let dir = tempfile::tempdir().unwrap();
    let base = json!({"lockfileVersion":"9.0",
        "importers":{".":{"dependencies":{"widget":{"specifier":"1.0.0","version":"1.0.0"}}}},
        "packages":{"widget@1.0.0":{"resolution":{"integrity":"sha512-widget"},"peerDependencies":{"host":"*"},"peerDependenciesMeta":{"host":{"optional":true}}}},
        "snapshots":{"widget@1.0.0":{}}});
    let mut main = base.clone();
    main["importers"]["."]["dependencies"]["extra"] =
        json!({"specifier":"1.0.0","version":"1.0.0"});
    main["packages"]["extra@1.0.0"] = json!({"resolution":{"integrity":"sha512-extra"}});
    main["snapshots"]["extra@1.0.0"] = json!({});
    let mut upgrade = base.clone();
    let bound = "1.0.0(host@2.0.0)";
    upgrade["importers"]["."]["dependencies"]["widget"]["version"] = json!(bound);
    upgrade["packages"]["host@2.0.0"] = json!({"resolution":{"integrity":"sha512-host"}});
    upgrade["snapshots"] =
        json!({"host@2.0.0":{},"widget@1.0.0(host@2.0.0)":{"dependencies":{"host":"2.0.0"}}});
    for (name, doc) in [("base", base), ("main", main), ("upgrade", upgrade)] {
        fs::write(dir.path().join(name), doc.to_string()).unwrap();
    }
    let older = json!({"packageManager":"pnpm@10.33.2"});
    let context = json!({
        "target":"ours",
        "manifests":{".":{"packageManager":"pnpm@12.21.0","dependencies":{"widget":"1.0.0","extra":"1.0.0"}}},
        "configurations":[older,older,{"packageManager":"pnpm@12.21.0"}],
        "configuration":{"linkWorkspacePackages":false,"pmOnFail":"ignore"}, "patches":{}
    });
    fs::write(dir.path().join("context.json"), context.to_string()).unwrap();
    let out = Command::new(CLI)
        .current_dir(dir.path())
        .args([
            "merge",
            "base",
            "main",
            "upgrade",
            "--context",
            "context.json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", report(&out));
    let merged = graph(&out.stdout);
    let deps = &merged["importers"]["."]["dependencies"];
    assert_eq!(deps["widget"]["version"], bound);
    assert_eq!(deps["extra"]["version"], "1.0.0");
    assert_eq!(
        merged["snapshots"]["widget@1.0.0(host@2.0.0)"]["dependencies"]["host"],
        "2.0.0"
    );
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
    let help = run(&["--help"]);
    let bare = run(&[]);
    assert!(help.status.success() && bare.status.success());
    assert!(!help.stdout.is_empty());
    assert_eq!(bare.stdout, help.stdout);
    assert!(bare.stderr.is_empty());
    for name in ["merge", "converge", "repair", "validate"] {
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
        &["converg"][..],
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

#[cfg(target_os = "linux")]
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
fn repair_uses_the_recorded_pnpm_version_for_workspace_configuration() {
    let repo = Repo::new();
    let project = repo.git(&["show", "HEAD:pnpm-lock.yaml"]);
    let version = "12.15.0";
    let lock = with_recorded_manager(version, &project);
    repo.write(
        "package.json",
        &json!({"devEngines": {"packageManager": {"name": "pnpm", "version": version, "onFail": "download"}}}).to_string(),
    );
    repo.write(".npmrc", "auto-install-peers=false\n");
    repo.write("pnpm-lock.yaml", &lock);
    repo.commit("record pnpm in the lock alongside a legacy setting");

    let out = repo.cmd(CLI, &["repair"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, lock.as_bytes());
}

#[test]
fn repair_uses_merge_parents_to_resolve_a_conflicting_snapshot() {
    const LOCK: &str = "pnpm-lock.yaml";
    let repo = Repo::new();
    repo.write("package.json", "{}");
    repo.write("pnpm-workspace.yaml", "packages: ['apps/*']\n");
    repo.write(".npmrc", "auto-install-peers=false\n");
    repo.write("apps/web/package.json", "{}");
    repo.write(
        LOCK,
        &with_recorded_manager(
            "12.15.0",
            "lockfileVersion: '9.0'\nimporters:\n  .: {}\n  apps/web: {}\n",
        ),
    );
    repo.commit("start without widget");
    repo.git(&["branch", "-f", "topic", "HEAD"]);

    let snapshot =
        |version: &str| format!("  widget@1.0.0:\n    dependencies:\n      child: {version}\n");
    let lock = |version: &str, snapshots: &str| {
        let project = format!(
            "# preserve the merged lock's formatting\n\
lockfileVersion: '9.0'\n\
importers:\n  .: {{}}\n  apps/web:\n    dependencies:\n      widget: {{specifier: 1.0.0, version: 1.0.0}}\n\
packages:\n  child@1.0.0: {{resolution: {{integrity: sha512-child-one}}}}\n  child@2.0.0: {{resolution: {{integrity: sha512-child-two}}}}\n  widget@1.0.0: {{resolution: {{integrity: sha512-widget}}}}\n\
snapshots:\n  child@1.0.0: {{}}\n  child@2.0.0: {{}}\n{snapshots}"
        );
        with_recorded_manager(version, &project)
    };
    let ours = snapshot("1.0.0");
    let theirs = snapshot("2.0.0");
    for (branch, snapshot) in [("main", &ours), ("topic", &theirs)] {
        repo.git(&["switch", branch]);
        repo.write(
            "apps/web/package.json",
            r#"{"dependencies":{"widget":"1.0.0"}}"#,
        );
        repo.write(LOCK, &lock("12.15.0", snapshot));
        repo.commit(branch);
    }
    repo.git(&["switch", "main"]);
    repo.git(&["merge", "-s", "ours", "--no-commit", "topic"]);
    repo.write("package.json", r#"{"packageManager":"pnpm@12.16.0"}"#);
    let damaged = lock("12.16.0", &format!("{theirs}{ours}"));
    repo.write(LOCK, &damaged);
    repo.commit("keep both snapshots in the merge result");
    let out = repo.cmd(CLI, &["repair"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, lock("12.16.0", &ours).as_bytes());
    assert_eq!(repo.git(&["show", "HEAD:pnpm-lock.yaml"]), damaged);

    repo.write("package.json", r#"{"packageManager":"pnpm@12.17.0"}"#);
    repo.git(&["commit", "-a", "--amend", "--no-edit"]);
    let out = repo.cmd(CLI, &["repair"]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    assert!(out.stdout.is_empty());
    assert!(
        report(&out).contains("bootstrap graph does not match the declared pnpm@12.17.0"),
        "{}",
        report(&out)
    );
}

#[test]
fn repair_reports_bootstrap_errors_before_loading_parent_projects() {
    let repo = Repo::new();
    let project = "lockfileVersion: '9.0'\nimporters: {.: {}}\n";
    repo.git(&["rm", "package.json"]);
    repo.write("pnpm-workspace.yaml", "packages: []\n");
    repo.write("pnpm-lock.yaml", &with_recorded_manager("12.18.0", project));
    repo.commit("record a lock without a manifest");
    repo.write("package.json", r#"{"packageManager":"pnpm@12.21.0"}"#);
    repo.write("pnpm-lock.yaml", &with_recorded_manager("12.20.0", project));
    repo.commit("restore the manifest with a different manager");

    let out = repo.cmd(CLI, &["repair"]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    assert!(out.stdout.is_empty());
    assert!(
        report(&out).contains("bootstrap graph does not match the declared pnpm@12.21.0"),
        "{}",
        report(&out)
    );
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

#[test]
fn repair_and_converge_honor_ignored_pnpmfiles_and_legacy_resolutions() {
    let repo = Repo::new();
    let original = repo.git(&["show", "HEAD:pnpm-lock.yaml"]);
    repo.write(".npmrc", "ignore-pnpmfile=true\n");
    repo.write(".pnpmfile.cjs", "module.exports = { hooks: {} };\n");
    repo.commit("ignore the workspace hook");
    let check = |expected: &str| {
        for args in [["repair", "HEAD"], ["converge", "HEAD"]] {
            let out = repo.cmd(CLI, &args);
            assert!(out.status.success(), "{}", report(&out));
            assert_eq!(out.stdout, expected.as_bytes());
        }
    };
    check(&original);

    repo.write(
        "package.json",
        &json!({"private":true,"packageManager":"pnpm@10.34.9",
            "resolutions":{"left":"2.0.0","right":"^1.0.0"},
            "pnpm":{"overrides":{"left":"1.0.0"}}})
        .to_string(),
    );
    let mut lock: Value = serde_json::from_str(&original).unwrap();
    lock["overrides"] = json!({"left":"1.0.0","right":"^1.0.0"});
    let text = lock.to_string();
    repo.write("pnpm-lock.yaml", &text);
    repo.commit("use legacy resolutions with an explicit override");
    check(&text);
}

#[test]
fn workspace_globs_can_contain_a_slash_in_a_character_class() {
    let repo = Repo::new();
    repo.write("pnpm-workspace.yaml", "packages: ['apps/[v/w]eb']\n");
    repo.commit("select web through a character class");
    for args in [["repair", "HEAD"], ["validate", "--workspace"]] {
        let out = repo.cmd(CLI, &args);
        assert!(out.status.success(), "{}", report(&out));
        if args[0] == "repair" {
            assert!(graph(&out.stdout)["importers"].get("apps/web").is_some());
        }
    }
}

#[test]
fn workspace_walks_follow_the_runners_character_class_semantics() {
    let repo = Repo::new();
    let names = ["web", "veb", "[v/w]eb", "fo/bar", "foobar", "foabar"];
    for (index, name) in names.iter().enumerate() {
        repo.write(
            &format!("apps/{name}/package.json"),
            &json!({"name":format!("pkg{index}")}).to_string(),
        );
    }
    let ordinary = ["web", "veb", "foobar", "foabar"];
    let bounded = &["apps/[v/w]eb", "apps/fo[o/a]bar"][..];
    let recursive = &["apps/**/[v/w]eb", "apps/**/fo[o/a]bar"][..];
    let excluded = &["apps/**", "!apps/[v/w]eb", "!apps/fo[o/a]bar"][..];
    let grouped = &["apps/{[v/w]eb,absent}", "apps/{fo[o/a]bar,absent}"][..];
    // Expected members from pnpm install --lockfile-only at the indicated releases.
    for (version, patterns, expected) in [
        ("10.5.2", bounded, &ordinary[..]),
        ("10.6.0", bounded, &names[..]),
        ("10.34.9", &["apps/{[a/../b],web}"], &[][..]),
        ("11.28.12", bounded, &names[..]),
        ("9.15.9", recursive, &names[..]),
        ("12.14.0", bounded, &ordinary[..]),
        ("12.15.0", bounded, &[][..]),
        ("12.21.0", recursive, &ordinary[..]),
        ("12.21.0", excluded, &["[v/w]eb", "fo/bar"][..]),
        ("12.21.0", grouped, &ordinary[..]),
    ] {
        repo.write(
            "package.json",
            &json!({"packageManager":format!("pnpm@{version}")}).to_string(),
        );
        repo.write(
            "pnpm-workspace.yaml",
            &json!({"packages":patterns,"pmOnFail":"ignore"}).to_string(),
        );
        let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{}}});
        for name in expected {
            lock["importers"][format!("apps/{name}")] = json!({});
        }
        repo.write("pnpm-lock.yaml", &lock.to_string());
        let out = repo.cmd(CLI, &["validate", "--workspace"]);
        assert!(
            out.status.success(),
            "{version} {patterns:?}: {}",
            report(&out)
        );
    }
}

#[test]
fn pnpm10_normalizes_parent_segments_before_matching_workspace_globs() {
    let repo = Repo::new();
    repo.write("pnpm-workspace.yaml", "packages: ['apps/[a/../b]']\n");
    repo.write("apps/b]/package.json", r#"{"name":"b"}"#);
    let lock = "lockfileVersion: '9.0'\nimporters: {.: {}, 'apps/b]': {}}\n";
    repo.write("pnpm-lock.yaml", lock);
    repo.commit("record the directory selected by pnpm 10.34.9");
    let validated = repo.cmd(CLI, &["validate", "--workspace"]);
    assert!(validated.status.success(), "{}", report(&validated));
    let repaired = repo.cmd(CLI, &["repair", "HEAD"]);
    assert!(repaired.status.success(), "{}", report(&repaired));
    assert_eq!(repaired.stdout, lock.as_bytes());
}

#[test]
fn normalized_workspace_globs_and_pnpm_manifest_formats_keep_healthy_importers() {
    let repo = Repo::new();
    let lock = repo.git(&["show", "HEAD:pnpm-lock.yaml"]);
    let web = repo.git(&["show", "HEAD:apps/web/package.json"]);
    repo.write(
        "pnpm-workspace.yaml",
        "packages: ['{apps,libs}/tmp/.././*']\n",
    );
    repo.write(
        "package.json",
        "\u{feff}{\"packageManager\":\"pnpm@10.34.9\"}",
    );
    repo.write("apps/web/package.json", &format!("\u{feff}{web}"));
    repo.write(
        "apps/web/package.json5",
        "// JSON5\n{name:'web',dependencies:{left:'1.0.0'},devDependencies:{right:'^1.0.0'},}",
    );
    repo.write("apps/web/package.yaml", "invalid: [");
    repo.commit("multiple manifest formats");
    for args in [["repair", "HEAD"], ["converge", "HEAD"]] {
        let out = repo.cmd(CLI, &args);
        assert!(out.status.success(), "{}", report(&out));
        assert_eq!(out.stdout, lock.as_bytes());
    }

    // The working tree and Git revision can use different manifest formats.
    repo.git(&["rm", "apps/web/package.json"]);
    let out = repo.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, lock.as_bytes());
    repo.commit("use JSON5");
    repo.git(&["rm", "apps/web/package.json5", "package.json"]);
    repo.write("package.yaml", "packageManager: pnpm@10.34.9\n");
    repo.write(
        "apps/web/package.yaml",
        "name: web\ndependencies: {left: 1.0.0}\ndevDependencies: {right: ^1.0.0}\n",
    );
    repo.commit("use YAML");
    for args in [["repair", "HEAD"], ["converge", "HEAD^"]] {
        let out = repo.cmd(CLI, &args);
        assert!(out.status.success(), "{}", report(&out));
        assert_eq!(out.stdout, lock.as_bytes());
    }
}

#[test]
fn repair_keeps_the_active_hooks_dependencies() {
    let repo = Repo::new();
    repo.write("package.json", r#"{"packageManager":"pnpm@12.21.0"}"#);
    repo.write(
        ".pnpmfile.mjs",
        "export const hooks = { readPackage(pkg) {return pkg} }\n",
    );
    repo.write(".pnpmfile.cjs", "module.exports = {};\n");
    // pnpm 12 records the SHA-256 of the selected .mjs hook.
    let mut lock: Value =
        serde_json::from_str(&repo.git(&["show", "HEAD:pnpm-lock.yaml"])).unwrap();
    lock["pnpmfileChecksum"] = json!("sha256-hcrgxuD4yzd6wArCpuECMwsXbpMreZHhRY4a0MX/XXA=");
    repo.write("pnpm-lock.yaml", &lock.to_string());
    repo.commit("record active hook");
    let out = repo.cmd(CLI, &["repair", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));

    repo.write(
        "apps/web/package.json",
        r#"{"name":"web","dependencies":{"left":"1.0.0"}}"#,
    );
    repo.commit("static declarations differ from the locked importer");
    let out = repo.cmd(CLI, &["repair", "HEAD"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(report(&out).contains("pnpmfile"), "{}", report(&out));
}

#[test]
fn repair_uses_the_hook_setting_supported_by_each_pnpm_version() {
    let repo = Repo::new();
    let lock: Value = serde_json::from_str(&repo.git(&["show", "HEAD:pnpm-lock.yaml"])).unwrap();
    repo.write(".pnpmfile.cjs", "module.exports = { hooks: {} };\n");

    // The checksums and active hooks below come from native pnpm lockfiles.
    const MD5: &str = "sfyfo37yjh2vk573iqozpdqqfa";
    const SHA256: &str = "sha256-dS522kUCN9FHUHk8JODaJjlMeNKVaNv8hwk1JhcGjEY=";
    let sources = [
        ("manifest", "", ""),
        ("npmrc", "ignore-pnpmfile=true\n", ""),
        (
            "npmrc inline comments",
            "ignore-pnpmfile=true ; disable the hook\nignore-pnpmfile=false # keep the hook\n",
            "",
        ),
        ("workspace YAML", "", "ignorePnpmfile: true\n"),
        (
            "workspace YAML over npmrc",
            "ignore-pnpmfile=true\n",
            "ignorePnpmfile: false\n",
        ),
    ];
    for (version, checksum, active_hooks) in [
        ("10.34.9", SHA256, [true, false, true, false, true]),
        ("9.15.9", MD5, [true, false, true, true, false]),
        ("10.5.0", SHA256, [true, false, true, true, false]),
        ("10.6.0", SHA256, [true, false, true, false, true]),
        ("11.0.0", SHA256, [true, true, true, false, true]),
        ("12.21.0", SHA256, [true, true, true, false, true]),
    ] {
        repo.write(
            "package.json",
            &json!({"private":true,"packageManager":format!("pnpm@{version}"),
                "pnpm":{"ignorePnpmfile":true}})
            .to_string(),
        );
        for ((source, npmrc, yaml), active) in sources.into_iter().zip(active_hooks) {
            repo.write(".npmrc", npmrc);
            repo.write(
                "pnpm-workspace.yaml",
                &format!("packages: ['apps/*']\npmOnFail: ignore\n{yaml}"),
            );
            let mut recorded = lock.clone();
            if active {
                recorded["pnpmfileChecksum"] = json!(checksum);
            }
            let text = recorded.to_string();
            repo.write("pnpm-lock.yaml", &text);
            repo.commit("record the effective pnpm hook");
            let out = repo.cmd(CLI, &["repair", "HEAD"]);
            assert!(
                out.status.success(),
                "{version}, {source}: {}",
                report(&out)
            );
            assert_eq!(out.stdout, text.as_bytes());
        }
    }
}

#[test]
fn stale_candidate_settings_are_reported_even_when_the_two_locks_match() {
    let repo = Repo::new();
    repo.write(
        "pnpm-workspace.yaml",
        "packages: ['apps/*']\npeersSuffixMaxLength: 32\n",
    );
    repo.commit("change only the suffix length");
    for args in [["converge", "HEAD"], ["repair", "HEAD"]] {
        let out = repo.cmd(CLI, &args);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(
            report(&out).contains("peersSuffixMaxLength"),
            "{}",
            report(&out)
        );
    }
}

#[test]
fn reading_a_partial_clone_leaves_unrelated_historical_blobs_on_the_remote() {
    check_partial_clone(false);
}

#[test]
fn converge_reads_missing_manifests_from_a_filter_only_remote() {
    check_partial_clone(true);
}

#[test]
fn a_blobless_clone_fetches_missing_workspace_manifests_together() {
    let source = Repo::new();
    let manifests = |version: &str| {
        for i in 0..12 {
            source.write(
                &format!("apps/pkg{i}/package.json"),
                &json!({"name":format!("pkg{i}"),"version":version}).to_string(),
            );
        }
    };
    manifests("1.0.0");
    source.commit("add workspace packages");
    let baseline = source.git(&["rev-parse", "HEAD"]);
    manifests("2.0.0");
    source.commit("change all workspace manifests");
    source.git(&["config", "uploadpack.allowFilter", "true"]);
    let clone = Repo(tempfile::tempdir().unwrap());
    source.git(&[
        "clone",
        "--no-local",
        "--filter=blob:none",
        source.0.path().to_str().unwrap(),
        clone.0.path().to_str().unwrap(),
    ]);
    let trace = clone.0.path().join("trace.jsonl");
    let result = Command::new(CLI)
        .current_dir(clone.0.path())
        .env("GIT_TRACE2_EVENT", &trace)
        .env("GIT_NO_LAZY_FETCH", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["repair", baseline.trim()])
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", report(&result));
    assert!(
        graph(&result.stdout)["importers"]
            .get("apps/pkg11")
            .is_some()
    );
    let events = fs::read_to_string(trace).unwrap();
    let sessions = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|e| e["event"] == "cmd_name" && e["name"] == "upload-pack")
        .count();
    assert_eq!(
        sessions, 1,
        "server sessions needed to retrieve twelve manifests"
    );
}

fn check_partial_clone(multiple_remotes: bool) {
    let repo = Repo::new();
    repo.write("unrelated/src.txt", "old source");
    repo.write("excluded/package.json", r#"{"name":"unused-old"}"#);
    repo.commit("baseline files");
    let baseline = repo.git(&["rev-parse", "HEAD"]);
    let other = repo.git(&["rev-parse", "HEAD:unrelated/src.txt"]);
    let excluded = repo.git(&["rev-parse", "HEAD:excluded/package.json"]);
    // All three historical blobs are missing after a blobless checkout.
    repo.write(
        "apps/web/package.json",
        &repo
            .git(&["show", "HEAD:apps/web/package.json"])
            .replace("web", "renamed"),
    );
    repo.write("unrelated/src.txt", "new source");
    repo.write("excluded/package.json", r#"{"name":"unused-new"}"#);
    repo.commit("change both members and unrelated files");
    repo.git(&["config", "uploadpack.allowFilter", "true"]);
    let clone = Repo(tempfile::tempdir().unwrap());
    let origin = repo.0.path().to_str().unwrap();
    repo.git(&[
        "clone",
        "--no-local",
        "--filter=blob:none",
        origin,
        clone.0.path().to_str().unwrap(),
    ]);
    if multiple_remotes {
        let empty = repo.0.path().join(".git/empty.git");
        let empty = empty.to_str().unwrap();
        repo.git(&["init", "--bare", empty]);
        clone.git(&["config", "--unset", "remote.origin.promisor"]);
        clone.git(&["remote", "add", "empty", empty]);
        clone.git(&["config", "remote.empty.promisor", "true"]);

        // Both remotes lack the manifest until origin points back to the source.
        clone.git(&["remote", "set-url", "origin", empty]);
        let failed = clone.cmd(CLI, &["converge", baseline.trim()]);
        assert!(!failed.status.success());
        assert!(failed.stdout.is_empty());
        clone.git(&["remote", "set-url", "origin", origin]);
    }
    let result = clone.cmd(CLI, &["converge", baseline.trim()]);
    assert!(result.status.success(), "{}", report(&result));
    let missing = clone.git(&["rev-list", "--objects", "--missing=print", baseline.trim()]);
    for oid in [other, excluded] {
        assert!(
            missing
                .lines()
                .any(|line| line.strip_prefix('?') == Some(oid.trim()))
        );
    }
}

#[test]
fn validate_checks_a_standalone_lockfile_without_writing_it() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(CLI)
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap()
    };
    let valid = "\
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      foo:
        specifier: 1.0.0
        version: 1.0.0
packages:
  foo@1.0.0:
    resolution: {tarball: https://example.org/foo-1.0.0.tgz}
snapshots:
  foo@1.0.0: {}
";
    let default_file = dir.path().join("pnpm-lock.yaml");
    fs::write(&default_file, valid).unwrap();
    let ok = run(&["validate"]);
    assert_eq!(ok.status.code(), Some(0), "{}", report(&ok));
    assert!(ok.stdout.is_empty() && ok.stderr.is_empty());
    assert_eq!(fs::read_to_string(default_file).unwrap(), valid);

    let candidate = dir.path().join("candidate.yaml");
    for (input, status, diagnostic) in [
        (format!("{valid}  foo@1.0.0: {{}}\n"), 1, "duplicate"),
        (
            valid.replace("snapshots:\n  foo@1.0.0: {}", "snapshots: {}"),
            1,
            "missing snapshot foo@1.0.0",
        ),
        (
            "lockfileVersion: '6.0'\n".into(),
            2,
            "unsupported lockfileVersion",
        ),
    ] {
        fs::write(&candidate, &input).unwrap();
        let result = run(&["validate", "candidate.yaml"]);
        assert_eq!(result.status.code(), Some(status), "{}", report(&result));
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains(diagnostic));
        assert_eq!(fs::read_to_string(&candidate).unwrap(), input);
    }
    let missing = run(&["validate", "missing.yaml"]);
    assert_eq!(missing.status.code(), Some(2), "{}", report(&missing));
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing.yaml"));
}

#[test]
fn validate_workspace_detects_manifest_and_membership_drift() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let member = root.join("apps/web");
    fs::create_dir_all(root.join("apps/example")).unwrap();
    fs::create_dir_all(&member).unwrap();
    write_input(&root, "package.json", r#"{"private":true}"#);
    write_input(&root, "apps/example/package.json", "{}");
    write_input(
        &root,
        "pnpm-workspace.yaml",
        "packages: ['apps/*', '!apps/example']\n",
    );
    let manifest = member.join("package.json");
    let current = r#"{"dependencies":{"foo":"^1.0.0"}}"#;
    fs::write(&manifest, current).unwrap();
    let lock = "\
lockfileVersion: '9.0'
importers:
  .: {}
  apps/web:
    dependencies:
      foo:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  foo@1.0.0:
    resolution: {tarball: https://example.org/foo-1.0.0.tgz}
snapshots:
  foo@1.0.0: {}
";
    let file = root.join("pnpm-lock.yaml");
    fs::write(&file, lock).unwrap();
    let run = |args: &[&str]| {
        Command::new(CLI)
            .current_dir(dir.path())
            .arg("validate")
            .args(args)
            .arg("project/pnpm-lock.yaml")
            .output()
            .unwrap()
    };
    let check = || run(&["--workspace"]);
    let expect_stale = |diagnostic: &str| {
        let result = check();
        assert_eq!(result.status.code(), Some(1), "{}", report(&result));
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains(diagnostic));
    };
    let result = check();
    assert!(result.status.success(), "{}", report(&result));
    assert!(result.stdout.is_empty() && result.stderr.is_empty());

    fs::write(&manifest, current.replace("^1.0.0", "^2.0.0")).unwrap();
    expect_stale("dependency foo");
    assert!(run(&[]).status.success());
    fs::write(&manifest, current).unwrap();

    let new_member = root.join("apps/new");
    fs::create_dir(&new_member).unwrap();
    fs::write(new_member.join("package.yaml"), "name: new\n").unwrap();
    expect_stale("missing importer");
    fs::remove_file(new_member.join("package.yaml")).unwrap();
    fs::remove_file(manifest).unwrap();
    expect_stale("absent project");
    assert_eq!(fs::read_to_string(file).unwrap(), lock);
}

#[test]
fn validate_checks_workspace_members_without_a_root_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name, manifest) in [
        (
            "app",
            r#"{"name":"app","dependencies":{"lib":"workspace:*"}}"#,
        ),
        ("lib", r#"{"name":"lib","version":"1.0.0"}"#),
    ] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("package.json"), manifest).unwrap();
    }
    write_input(root, "pnpm-workspace.yaml", "packages: [app, lib]\n");
    let mut lock = json!({"lockfileVersion":"9.0","importers":{
        "app":{"dependencies":{"lib":{"specifier":"workspace:*","version":"link:../lib"}}},"lib":{}
    }});
    write_input(root, "pnpm-lock.yaml", &lock);
    check_workspace(root, 0, ());

    lock["importers"]["."] = json!({});
    write_input(root, "pnpm-lock.yaml", &lock);
    let result = check_workspace(root, 1, ());
    assert!(String::from_utf8_lossy(&result.stderr).contains("absent project"));
}

#[test]
fn validate_checks_the_member_selected_by_a_workspace_link() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(
        root,
        "pnpm-workspace.yaml",
        "packages: [lib, other]\ncatalog: {alias: 'workspace:@local/lib@^1'}\n",
    );
    let member =
        json!({"name":"@local/lib","version":"1.2.0","publishConfig":{"directory":"dist"}});
    fs::create_dir(root.join("lib")).unwrap();
    fs::create_dir(root.join("other")).unwrap();
    write_input(root, "lib/package.json", &member);
    write_input(
        root,
        "other/package.json",
        r#"{"name":"@local/lib","version":"2.0.0"}"#,
    );
    for (name, specifier, target, expected) in [
        ("@local/lib", "workspace:^1", "lib/dist", 0),
        ("@local/lib", "workspace:^1", "other", 1),
        ("alias", "workspace:@local/lib@^1", "lib/dist", 0),
        ("alias", "catalog:", "lib/dist", 0),
        ("alias", "catalog:", "other", 1),
        ("renamed", "workspace:*", "lib/dist", 1),
        ("alias", "workspace:@local/lib@latest", "lib/dist", 0),
        ("alias", "workspace:./lib", "lib", 0),
        ("alias", "workspace:./lib", "other", 1),
        ("alias", "workspace:./unbuilt", "unbuilt", 0),
        ("alias", "link:../external", "../external", 0),
    ] {
        write_input(
            root,
            "package.json",
            json!({"dependencies":{name:specifier}}),
        );
        let lock = json!({"lockfileVersion":"9.0","importers":{
            ".":{"dependencies":{name:{"specifier":specifier,"version":format!("link:{target}")}}},
            "lib":{"publishDirectory":"dist"},"other":{}
        }});
        write_input(root, "pnpm-lock.yaml", &lock);
        let result = check_workspace(root, expected, (name, specifier, target));
        if expected == 1 {
            assert!(String::from_utf8_lossy(&result.stderr).contains(name));
        }
    }
}

#[test]
fn validate_allows_omitted_implicit_workspace_links_supported_by_pnpm() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("lib")).unwrap();
    write_input(
        root,
        "lib/package.json",
        r#"{"name":"lib","version":"1.0.0"}"#,
    );
    for (pnpm, linking, name, spec, status) in [
        ("10.33.2", true, "lib", "^1", 1),
        ("11.28.10", true, "lib", "^1", 1),
        ("11.28.11", true, "lib", "^1", 0),
        ("11.28.11", true, "lib", "catalog:", 0),
        ("12.5.1", true, "lib", "^1", 1),
        ("12.6.0", true, "alias", "npm:lib@^1", 0),
        ("12.6.0", true, "lib", "catalog:", 1),
        ("12.18.0", true, "lib", "catalog:", 0),
        ("12.18.0", true, "lib", "^2", 1),
        ("12.18.0", true, "external", "^1", 1),
        ("12.18.0", true, "lib", "workspace:^1", 1),
        ("12.18.0", false, "lib", "^1", 1),
    ] {
        write_input(
            root,
            "package.json",
            json!({"packageManager":format!("pnpm@{pnpm}"),"dependencies":{name:spec}}),
        );
        write_input(
            root,
            "pnpm-workspace.yaml",
            format!(
                "packages: [lib]\npmOnFail: ignore\nexcludeLinksFromLockfile: true\nlinkWorkspacePackages: {linking}\ncatalog: {{lib: '^1'}}\n"
            ),
        );
        let lock = json!({"lockfileVersion":"9.0","settings":{"excludeLinksFromLockfile":true},"importers":{".":{},"lib":{}}});
        write_input(root, "pnpm-lock.yaml", &lock);
        check_workspace(root, status, (pnpm, name, spec));
    }
}

#[test]
fn validate_requires_a_local_resolution_for_workspace_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(
        root,
        "pnpm-workspace.yaml",
        "packages: [app, lib]\ncatalog: {alias: 'workspace:lib@^1'}\n",
    );
    write_input(root, "package.json", "{}");
    fs::create_dir(root.join("app")).unwrap();
    fs::create_dir(root.join("lib")).unwrap();
    write_input(
        root,
        "lib/package.json",
        r#"{"name":"lib","version":"1.2.0"}"#,
    );
    let registry = ("1.2.0", "lib@1.2.0", None);
    let local = ("file:lib", "lib@file:lib", Some("lib"));
    let alias = ("lib@file:lib", "lib@file:lib", Some("lib"));
    let wrong_name = ("other@file:lib", "other@file:lib", Some("lib"));
    let wrong_target = ("file:app", "lib@file:app", Some("app"));
    let wrong_record = ("file:lib", "lib@file:lib", Some("app"));
    for (name, specifier, (pin, key, directory), expected) in [
        ("lib", "^1", registry, 0),
        ("lib", "workspace:^1", registry, 1),
        ("lib", "workspace:^1", local, 0),
        ("alias", "workspace:lib@^1", alias, 0),
        ("alias", "catalog:", alias, 0),
        ("alias", "workspace:../lib", alias, 0),
        ("lib", "workspace:^2", local, 1),
        ("alias", "workspace:lib@^1", wrong_name, 1),
        ("alias", "workspace:../lib", wrong_name, 1),
        ("lib", "workspace:^1", wrong_target, 1),
        ("lib", "workspace:^1", wrong_record, 1),
    ] {
        let meta = json!({name:{"injected":true}});
        write_input(
            root,
            "app/package.json",
            json!({"dependencies":{name:specifier},"dependenciesMeta":meta}),
        );
        let resolution = directory.map_or_else(
            || json!({"integrity":"sha512-abc"}),
            |path| json!({"type":"directory","directory":path}),
        );
        let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{},"lib":{},"app":{
            "dependencies":{name:{"specifier":specifier,"version":pin}},"dependenciesMeta":meta
        }},"packages":{key:{"resolution":resolution}},"snapshots":{key:{}}});
        if specifier == "catalog:" {
            lock["catalogs"] =
                json!({"default":{"alias":{"specifier":"workspace:lib@^1","version":"file:lib"}}});
        }
        write_input(root, "pnpm-lock.yaml", &lock);
        check_workspace(root, expected, (name, specifier, pin));
    }
}

#[test]
fn validate_compares_unused_recorded_catalog_specifiers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    let lock = json!({"lockfileVersion":"9.0","importers":{".":{}},"catalogs":{
        "tools":{"foo":{"specifier":"^1","version":"1.2.0"}}
    }});
    write_input(root, "pnpm-lock.yaml", &lock);
    for (config, expected) in [
        ("catalogs: {tools: {foo: '^1', bar: '*'}}", 0),
        ("catalogs: {tools: {foo: '^1.1'}}", 1),
        ("catalog: {foo: '^1'}", 1),
    ] {
        write_input(
            root,
            "pnpm-workspace.yaml",
            format!("packages: []\n{config}\n"),
        );
        let result = check_workspace(root, expected, config);
        if expected == 1 {
            assert!(String::from_utf8_lossy(&result.stderr).contains("tools/foo"));
        }
    }
}

#[test]
fn validate_uses_the_recorded_manager_for_settings_and_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    write_input(
        root,
        ".npmrc",
        "auto-install-peers=false\nignore-pnpmfile=true\n",
    );
    let project = json!({"lockfileVersion": "9.0", "importers": {".": {}}});
    write_input(
        root,
        "pnpm-lock.yaml",
        with_recorded_manager("11.28.12", &format!("{project}\n")),
    );
    check_workspace(root, 0, ());

    write_input(root, ".pnpmfile.mjs", "export const hooks = {}\n");
    let result = check_workspace(root, 2, ());
    assert!(String::from_utf8_lossy(&result.stderr).contains("effective manifests"));
}

fn with_recorded_manager(version: &str, project: &str) -> String {
    let environment = json!({
        "lockfileVersion": "9.0",
        "importers": {".": {"packageManagerDependencies": {"pnpm": {"specifier": version, "version": version}}}},
        "packages": {format!("pnpm@{version}"): {"resolution": {}}},
        "snapshots": {format!("pnpm@{version}"): {}}
    });
    format!("---\n{environment}\n---\n{project}")
}

#[test]
fn validate_rejects_a_dedupe_peers_setting_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    write_input(
        root,
        "pnpm-workspace.yaml",
        "packages: []\ndedupePeers: true\n",
    );
    for (settings, status) in [
        ("", 1),
        ("settings: {dedupePeers: false}\n", 1),
        ("settings: {dedupePeers: true}\n", 0),
    ] {
        write_input(
            root,
            "pnpm-lock.yaml",
            format!("lockfileVersion: '9.0'\n{settings}importers: {{.: {{}}}}\n"),
        );
        check_workspace(root, status, settings);
    }
}

#[test]
fn validate_rejects_mismatched_recorded_resolution_settings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut lock = json!({"lockfileVersion": "9.0", "importers": {".": {}}});
    for (version, field, default, configured) in [
        (
            "10.33.2",
            "injectWorkspacePackages",
            json!(false),
            json!(true),
        ),
        ("10.33.2", "peersSuffixMaxLength", json!(1000), json!(750)),
        ("12.15.0", "addMissingPeerTypes", json!(false), json!(true)),
    ] {
        write_input(
            root,
            "package.json",
            json!({"packageManager": format!("pnpm@{version}")}),
        );
        write_input(
            root,
            "pnpm-workspace.yaml",
            format!("packages: []\npmOnFail: ignore\n{field}: {configured}\n"),
        );
        for (value, status) in [
            (json!({}), 1),
            (json!({field: default}), 1),
            (json!({field: configured}), 0),
        ] {
            lock["settings"] = value;
            write_input(root, "pnpm-lock.yaml", &lock);
            let result = check_workspace(root, status, field);
            if status == 1 {
                assert!(String::from_utf8_lossy(&result.stderr).contains(field));
            }
        }
    }
}

#[test]
fn validate_detects_added_changed_and_removed_package_extensions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", r#"{"packageManager":"pnpm@10.33.2"}"#);
    // Generated by pnpm 10.33.2 for the extension below.
    let checksum = "sha256-gFzag6IRsulpdTRdkb1wLQgmLSEoJT1a56wBrteXILs=";
    let extension = "packageExtensions:\n  'unused-probe@*':\n    dependencies: {dummy: '1.0.0'}\n";
    let changed = extension.replace("1.0.0", "2.0.0");
    for (recorded, configured, status) in [
        (Some(checksum), extension, 0),
        (None, extension, 1),
        (Some(checksum), changed.as_str(), 1),
        (Some(checksum), "", 1),
        (None, "packageExtensions: {}\n", 0),
        (None, "packageExtensions: null\n", 0),
        (Some(checksum), "packageExtensions: null\n", 1),
    ] {
        let mut lock = json!({"lockfileVersion": "9.0", "importers": {".": {}}});
        if let Some(recorded) = recorded {
            lock["packageExtensionsChecksum"] = json!(recorded);
        }
        write_input(root, "pnpm-lock.yaml", &lock);
        write_input(
            root,
            "pnpm-workspace.yaml",
            format!("packages: []\n{configured}"),
        );
        let result = check_workspace(root, status, ());
        if status == 1 {
            assert!(String::from_utf8_lossy(&result.stderr).contains("packageExtensionsChecksum"));
        }
    }
}

#[test]
fn validate_compares_patch_declarations_and_contents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let contents = "diff --git a/index.js b/index.js\n--- a/index.js\n+++ b/index.js\n@@ -1 +1 @@\n-old\n+new\n";
    // Recorded by pnpm 10.33.2 and 12.15.0 for the patch above.
    let hash = "39d1d17e986e1d5203cab3cb5cf582396a95b3b12c3d7ae535677c53afca149c";
    for (manifest, record) in [
        (
            json!({"packageManager":"pnpm@10.33.2"}),
            json!({"hash":hash,"path":"probe.patch"}),
        ),
        (json!({}), json!(hash)),
    ] {
        let lock = json!({"lockfileVersion":"9.0","importers":{".":{}},"patchedDependencies":{"unused@1.0.0":record}});
        write_input(root, "package.json", &manifest);
        write_input(root, "pnpm-lock.yaml", &lock);
        write_input(root, "probe.patch", contents);
        write_input(
            root,
            "pnpm-workspace.yaml",
            "packages: []\npatchedDependencies: {'unused@1.0.0': ./probe.patch}\n",
        );
        check_workspace(root, 0, ());

        write_input(root, "probe.patch", contents.replace("+new", "+edited"));
        let result = check_workspace(root, 1, ());
        assert!(String::from_utf8_lossy(&result.stderr).contains("patchedDependencies"));

        write_input(root, "pnpm-workspace.yaml", "packages: []\n");
        let result = check_workspace(root, 1, ());
        assert!(String::from_utf8_lossy(&result.stderr).contains("patchedDependencies"));
    }
}

#[test]
fn validate_filters_ignored_optional_declarations_before_selecting_the_dependency_group() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let pin = json!({"specifier":"^1","version":"1.0.0"});
    let optional = json!({"optionalDependencies":{"foo":"^1"}});
    let peer = json!({"optionalDependencies":{"foo":"*"},"peerDependencies":{"foo":"^1"}});
    let dev = json!({"optionalDependencies":{"foo":"*"},"devDependencies":{"foo":"^1"}});
    let both = json!({"optionalDependencies":{"foo":"^1"},"dependencies":{"foo":"*"}});
    let prod_group = Some("dependencies");
    let opt_group = Some("optionalDependencies");
    for (manager, manifest, rules, group, status) in [
        ("10.28.1", &optional, vec!["foo"], None, 0),
        ("10.28.1", &both, vec!["f*"], None, 0),
        ("10.28.1", &dev, vec!["foo"], Some("devDependencies"), 0),
        ("10.28.1", &peer, vec!["foo"], prod_group, 0),
        ("12.15.0", &peer, vec!["foo"], prod_group, 1),
        ("12.15.0", &peer, vec!["foo"], None, 0),
        ("10.28.1", &optional, vec!["f*", "!foo"], opt_group, 0),
        ("11.21.0", &optional, vec!["f*", "!foo"], opt_group, 1),
        ("10.28.1", &optional, vec!["!foo", "f*"], None, 0),
        (
            "10.28.1",
            &json!({"dependencies":{"foo":"^1"}}),
            vec!["foo"],
            None,
            1,
        ),
        ("10.28.1", &optional, vec!["bar"], None, 1),
        ("10.28.1", &optional, vec!["foo"], opt_group, 1),
    ] {
        let mut manifest = manifest.clone();
        manifest["packageManager"] = json!(format!("pnpm@{manager}"));
        write_input(root, "package.json", &manifest);
        write_input(
            root,
            "pnpm-workspace.yaml",
            json!({"packages":[], "pmOnFail":"ignore", "ignoredOptionalDependencies":rules}),
        );
        let mut recorded = rules.clone();
        recorded.sort_unstable();
        let mut lock = json!({"lockfileVersion":"9.0","ignoredOptionalDependencies":recorded,"importers":{".":{}}});
        if let Some(group) = group {
            lock["importers"]["."][group] = json!({"foo":pin});
            lock["packages"] = json!({"foo@1.0.0":{"resolution":{}}});
            lock["snapshots"] = json!({"foo@1.0.0":{}});
        }
        write_input(root, "pnpm-lock.yaml", &lock);
        check_workspace(root, status, (&manifest, &rules));
    }
}

#[cfg(unix)]
#[test]
fn validate_follows_workspace_directory_links_and_preserves_importer_paths() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let external = dir.path().join("external");
    fs::create_dir_all(external.join("nested")).unwrap();
    fs::create_dir_all(root.join("packages/internal")).unwrap();
    for path in [external.join("nested"), root.join("packages/internal")] {
        fs::write(
            path.join("package.json"),
            r#"{"name":"lib","version":"1.0.0"}"#,
        )
        .unwrap();
    }
    symlink(&external, root.join("linked")).unwrap();
    symlink("packages/internal", root.join("alias")).unwrap();
    symlink(&root, external.join("back")).unwrap();
    let nested = "linked/nested";
    let dirs = &["linked/**"][..];
    let aliases = &["alias", "packages/*"][..];
    for (manager, globs, member, spec, has_importer, status) in [
        ("10.28.1", dirs, nested, "workspace:^1", false, 0),
        ("10.28.1", dirs, nested, "workspace:^2", false, 1),
        ("10.28.1", aliases, "alias", "workspace:^1", true, 0),
        ("10.28.1", &["alias"], "alias", "workspace:^1", false, 1),
        ("12.7.0", dirs, nested, "workspace:^1", false, 1),
        ("12.8.0", dirs, nested, "workspace:^1", false, 0),
        ("12.14.0", dirs, nested, "workspace:^1", true, 0),
        ("12.15.0", dirs, nested, "workspace:^1", true, 1),
        (
            "12.15.0",
            dirs,
            nested,
            "workspace:./linked/nested",
            true,
            0,
        ),
    ] {
        write_input(
            &root,
            "package.json",
            json!({"packageManager":format!("pnpm@{manager}"),"dependencies":{"lib":spec}}),
        );
        write_input(
            &root,
            "pnpm-workspace.yaml",
            json!({"packages":globs,"pmOnFail":"ignore"}),
        );
        let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"lib":{"specifier":spec,"version":format!("link:{member}")}}}}});
        if has_importer {
            lock["importers"][member] = json!({});
        }
        if globs.contains(&"packages/*") {
            lock["importers"]["packages/internal"] = json!({});
        }
        write_input(&root, "pnpm-lock.yaml", &lock);
        check_workspace(&root, status, (manager, member, spec));
    }
    write_input(
        &root,
        "package.json",
        json!({"packageManager":"pnpm@12.19.0","dependencies":{"lib":"^1"}}),
    );
    write_input(
        &root,
        "pnpm-workspace.yaml",
        "packages: ['linked/*']\npmOnFail: ignore\nlinkWorkspacePackages: true\nexcludeLinksFromLockfile: true",
    );
    write_input(
        &root,
        "pnpm-lock.yaml",
        json!({"lockfileVersion":"9.0","settings":{"excludeLinksFromLockfile":true},"importers":{".":{},"linked/nested":{}}}),
    );
    check_workspace(&root, 0, "excluded implicit workspace link");
    write_input(&root, "pnpm-workspace.yaml", "packages: ['linked/*']");
    fs::write(
        external.join("nested/package.json"),
        r#"{"name":"lib","version":"1.0.0","dependencies":{"child":"link:./child"}}"#,
    )
    .unwrap();
    write_input(
        &root,
        "package.json",
        r#"{"packageManager":"pnpm@10.28.1"}"#,
    );
    write_input(
        &root,
        "pnpm-lock.yaml",
        r#"{"lockfileVersion":"9.0","importers":{".":{}}}"#,
    );
    assert!(report(&check_workspace(&root, 1, ())).contains("child"));
}

#[test]
fn legacy_pnpm_keeps_workspace_members_when_dev_engines_names_npm() {
    let repo = Repo::new();
    repo.write("pnpm-workspace.yaml", "{}\n");
    for version in ["9.15.9", "10.4.1"] {
        let manifest = json!({"packageManager":format!("pnpm@{version}"),
            "devEngines":{"packageManager":{"name":"npm","version":"^11"}}});
        repo.write("package.json", &manifest.to_string());
        repo.commit("record a legacy pnpm workspace with npm engine metadata");
        check_workspace(repo.0.path(), 0, version);
        let repaired = repo.cmd(CLI, &["repair", "HEAD"]);
        assert!(repaired.status.success(), "{}", report(&repaired));
        assert!(
            graph(&repaired.stdout)["importers"]
                .get("apps/web")
                .is_some()
        );
    }
}

#[test]
fn a_locked_modern_manager_uses_dev_engines_over_a_legacy_pin() {
    let repo = Repo::new();
    repo.write("package.json", r#"{"packageManager":"pnpm@9.15.9","devEngines":{"packageManager":{"name":"pnpm","version":"^12.20.0"}}}"#);
    let project = repo.git(&["show", "HEAD:pnpm-lock.yaml"]);
    let input = with_recorded_manager("12.21.0", &project);
    repo.write("pnpm-workspace.yaml", "packages: ['apps/*']\n");
    repo.write("pnpm-lock.yaml", &input);
    repo.commit("record the package manager selected by devEngines");
    check_workspace(repo.0.path(), 0, "modern bootstrap");
    let out = repo.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, input.as_bytes());
    let manifest: Value = serde_json::from_str(&repo.git(&["show", "HEAD:package.json"])).unwrap();
    let member: Value =
        serde_json::from_str(&repo.git(&["show", "HEAD:apps/web/package.json"])).unwrap();
    for config in [json!({}), json!({"packageManager":"pnpm@12.21.0"})] {
        let context = json!({"target":"ours","manifests":{".":manifest,"apps/web":member},
            "configurations":[{},{},{}],"configuration":config,"patches":{}});
        repo.write("ctx.json", &context.to_string());
        let out = repo.cmd(
            CLI,
            &[
                "merge",
                "pnpm-lock.yaml",
                "pnpm-lock.yaml",
                "pnpm-lock.yaml",
                "--context",
                "ctx.json",
            ],
        );
        assert!(out.status.success(), "{}", report(&out));
        assert_eq!(graph(&out.stdout), graph(project.as_bytes()));
        let merged = Lockfile::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
        assert_eq!(
            merged.recorded_pnpm_version().unwrap().to_string(),
            "12.21.0"
        );
    }
}

#[cfg(unix)]
#[test]
fn parallel_workspace_validation_reports_the_first_error_without_waiting_on_a_fifo() {
    use std::{
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    write_input(root, "pnpm-workspace.yaml", "packages: ['apps/*']");
    write_input(
        root,
        "pnpm-lock.yaml",
        "lockfileVersion: '9.0'\nimporters: {.: {}}\n",
    );
    for index in 0..31 {
        let manifest = if index == 0 { "{invalid" } else { "{}" };
        write_input(root, &format!("apps/{index:02}/package.json"), manifest);
    }
    fs::create_dir(root.join("apps/31")).unwrap();
    let fifo = Command::new("mkfifo")
        .arg(root.join("apps/31/package.json"))
        .output()
        .unwrap();
    assert!(fifo.status.success(), "{}", report(&fifo));

    let mut process = Command::new(CLI)
        .current_dir(root)
        .args(["validate", "--workspace"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            process.kill().unwrap();
            process.wait().unwrap();
            panic!("workspace validation blocked on a manifest FIFO");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let out = process.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert!(
        report(&out).contains("apps/00/package.json"),
        "{}",
        report(&out)
    );
}

#[test]
fn validate_discovers_only_the_members_selected_by_the_pnpm_version_and_globs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("app");
    for path in [root.join("internal"), dir.path().join("shared")] {
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("package.json"),
            r#"{"name":"lib","version":"1.0.0"}"#,
        )
        .unwrap();
    }
    for (version, yaml, members, code) in [
        ("", "{}", vec![], 0),
        ("", "packages: ['internal']", vec!["internal"], 0),
        ("9.15.9", "{}", vec!["internal"], 0),
        ("10.4.1", "{}", vec!["internal"], 0),
        ("10.4.1", "linkWorkspacePackages: true", vec![], 2),
        ("10.5.0", "{}", vec![], 0),
        ("12.18.0", "linkWorkspacePackages: true", vec![], 0),
        ("12.18.0", "packages: ['**']", vec!["internal"], 0),
        ("12.18.0", "packages: ['../shared']", vec!["../shared"], 0),
        ("12.18.0", "packages: ['../*']", vec!["../shared"], 0),
        ("12.18.0", "packages: ['../sha*']", vec![], 1),
    ] {
        let mut manifest = json!({"pnpm":{"packages":["unused"]}});
        if !version.is_empty() {
            manifest["packageManager"] = json!(format!("pnpm@{version}"));
        }
        let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{}}});
        for &id in &members {
            lock["importers"][id] = json!({});
        }
        if members.contains(&"../shared") {
            manifest["dependencies"] = json!({"lib":"workspace:*"});
            lock["importers"]["."]["dependencies"] =
                json!({"lib":{"specifier":"workspace:*","version":"link:../shared"}});
        }
        write_input(&root, "package.json", &manifest);
        write_input(&root, "pnpm-workspace.yaml", yaml);
        let project = lock.to_string();
        let lock = if version.starts_with("12.") {
            with_recorded_manager(version, &project)
        } else {
            project
        };
        write_input(&root, "pnpm-lock.yaml", lock);
        check_workspace(&root, code, (version, yaml));
    }
}

#[cfg(unix)]
#[test]
fn validate_resolves_only_symlinks_selected_by_a_workspace_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    write_input(
        root,
        "pnpm-lock.yaml",
        "lockfileVersion: '9.0'\nimporters: {.: {}}\n",
    );
    std::os::unix::fs::symlink("scratch", root.join("scratch")).unwrap();
    for (pattern, code) in [
        ("app", 0),
        ("'*', '!scratch'", 0),
        ("'**', '!scratch/**'", 0),
        ("scratch", 0),
    ] {
        write_input(
            root,
            "pnpm-workspace.yaml",
            format!("packages: [{pattern}]"),
        );
        check_workspace(root, code, ());
    }
}

#[test]
fn validate_checks_recorded_range_links_for_the_releases_that_verify_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("lib")).unwrap();
    for (version, enabled, spec, target_version, code) in [
        ("12.18.0", true, "^1", "2.0.0", 0),
        ("12.19.0", true, "^1", "2.0.0", 1),
        ("12.19.0", true, "^1", "1.1.0", 0),
        ("12.19.0", true, "npm:lib@^1", "2.0.0", 1),
        ("12.19.0", true, "npm:^1", "1.1.0", 0),
        ("11.28.12", true, "npm:^1", "2.0.0", 1),
        ("12.19.0", false, "^1", "2.0.0", 0),
        ("12.19.0", true, "link:lib", "2.0.0", 0),
        ("11.27.1", true, "^1", "2.0.0", 0),
        ("11.28.0", true, "^1", "2.0.0", 1),
    ] {
        write_input(
            root,
            "lib/package.json",
            json!({"name":"lib","version":target_version}),
        );
        write_input(
            root,
            "package.json",
            json!({"packageManager":format!("pnpm@{version}"),"dependencies":{"lib":spec}}),
        );
        for members in ["lib", ""] {
            write_input(
                root,
                "pnpm-workspace.yaml",
                format!(
                    "packages: [{members}]\npmOnFail: ignore\nlinkWorkspacePackages: {enabled}\n"
                ),
            );
            let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"lib":{"specifier":spec,"version":"link:lib"}}}}});
            if !members.is_empty() {
                lock["importers"][members] = json!({});
            }
            write_input(root, "pnpm-lock.yaml", &lock);
            check_workspace(root, code, (version, spec, members));
        }
    }
}

#[test]
fn validate_compares_ignored_optional_dependencies_as_an_unordered_list() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    for (recorded, configured, status) in [
        ("", "[]", 0),
        ("", "[fsevents]", 1),
        (
            "ignoredOptionalDependencies: [fsevents, canvas]\n",
            "[canvas, fsevents]",
            0,
        ),
        ("ignoredOptionalDependencies: [fsevents]\n", "[]", 1),
    ] {
        write_input(
            root,
            "pnpm-lock.yaml",
            format!("lockfileVersion: '9.0'\n{recorded}importers: {{.: {{}}}}\n"),
        );
        write_input(
            root,
            "pnpm-workspace.yaml",
            format!("packages: []\nignoredOptionalDependencies: {configured}\n"),
        );
        let result = check_workspace(root, status, ());
        if status == 1 {
            assert!(
                String::from_utf8_lossy(&result.stderr).contains("ignoredOptionalDependencies")
            );
        }
    }
}

#[test]
fn validate_rejects_reordered_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    let rules = [
        "  'foo@^1.0.0': '2.0.0'\n",
        "  'foo@>=1.0.0 <2.0.0': '3.0.0'\n",
    ];
    write_input(
        root,
        "pnpm-lock.yaml",
        format!(
            "lockfileVersion: '9.0'\nimporters: {{.: {{}}}}\noverrides:\n{}{}",
            rules[0], rules[1]
        ),
    );
    for (first, second, status) in [(rules[0], rules[1], 0), (rules[1], rules[0], 1)] {
        write_input(
            root,
            "pnpm-workspace.yaml",
            format!("packages: []\noverrides:\n{first}{second}"),
        );
        let result = check_workspace(root, status, ());
        if status == 1 {
            assert!(String::from_utf8_lossy(&result.stderr).contains("overrides differ"));
        }
    }
}

fn write_input(root: &Path, name: &str, contents: impl std::fmt::Display) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents.to_string()).unwrap();
}

#[test]
fn converge_tracks_an_installed_git_ignored_member_from_addition_to_removal() {
    let r = Repo::new();
    r.write(
        "package.json",
        r#"{"private":true,"packageManager":"pnpm@12.21.0"}"#,
    );
    r.write(".gitignore", "apps/generated/\n");
    r.commit("ignore generated files");
    let mut manifest = json!({"dependencies":{"local":"link:../local"},
        "dependenciesMeta":{"local":{"injected":false}},
        "publishConfig":{"directory":"dist","linkDirectory":false}});
    r.write("apps/generated/package.json", &manifest.to_string());
    let mut candidate = graph(&fs::read(r.0.path().join("pnpm-lock.yaml")).unwrap());
    candidate["importers"]["apps/generated"] = json!({"dependencies":{"local":{"specifier":"link:../local","version":"link:../local"}},
            "dependenciesMeta":manifest["dependenciesMeta"],"publishDirectory":"dist","linkDirectory":false});
    r.write("pnpm-lock.yaml", &candidate.to_string());
    check_workspace(r.0.path(), 0, "installed candidate");
    let result = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(result.status.success(), "{}", report(&result));
    r.write(
        "pnpm-lock.yaml",
        std::str::from_utf8(&result.stdout).unwrap(),
    );
    check_workspace(r.0.path(), 0, "converged output");
    r.commit("record the generated importer");
    let repeat = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(repeat.status.success(), "{}", report(&repeat));
    assert_eq!(repeat.stdout, result.stdout);
    let new_link = "link:../other";
    manifest["dependencies"]["local"] = json!(new_link);
    manifest["publishConfig"] = json!({"directory":"build"});
    r.write("apps/generated/package.json", &manifest.to_string());
    candidate["importers"]["apps/generated"]["publishDirectory"] = json!("build");
    candidate["importers"]["apps/generated"]
        .as_object_mut()
        .unwrap()
        .remove("linkDirectory");
    candidate["importers"]["apps/generated"]["dependencies"]["local"] =
        json!({"specifier":new_link,"version":new_link});
    r.write("pnpm-lock.yaml", &candidate.to_string());
    let edit = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(edit.status.success(), "{}", report(&edit));
    r.write("pnpm-lock.yaml", std::str::from_utf8(&edit.stdout).unwrap());
    check_workspace(r.0.path(), 0, "edited generated member");
    r.commit("update generated member");
    fs::remove_dir_all(r.0.path().join("apps/generated")).unwrap();
    candidate["importers"]
        .as_object_mut()
        .unwrap()
        .remove("apps/generated");
    r.write("pnpm-lock.yaml", &candidate.to_string());
    let removed = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(removed.status.success(), "{}", report(&removed));
    r.write(
        "pnpm-lock.yaml",
        std::str::from_utf8(&removed.stdout).unwrap(),
    );
    check_workspace(r.0.path(), 0, "removed generated member");
}

#[test]
fn converge_uses_recorded_specifiers_once_for_generated_members() {
    let r = Repo::new();
    r.write(".gitignore", "apps/generated/\n");
    let overrides = json!({"is-number@5.0.0":"6.0.0","is-number@6.0.0":"7.0.0"});
    r.write(
        "pnpm-workspace.yaml",
        &json!({"packages":["apps/*"],"overrides":overrides}).to_string(),
    );
    let id = "apps/generated";
    let path = "apps/generated/package.json";
    let mut lock = graph(&fs::read(r.0.path().join("pnpm-lock.yaml")).unwrap());
    lock["overrides"] = overrides.clone();
    let importer = |version: &str| json!({"dependencies":{"is-number":{"specifier":version,"version":version}}});
    let manifest = |version: &str| json!({"dependencies":{"is-number":version}}).to_string();
    r.write(path, &manifest("5.0.0"));
    lock["importers"][id] = importer("6.0.0");
    lock["packages"]["is-number@6.0.0"] = json!({"resolution":{}});
    lock["snapshots"]["is-number@6.0.0"] = json!({});
    r.write("pnpm-lock.yaml", &lock.to_string());
    check_workspace(r.0.path(), 0, "baseline");
    r.commit("record the overridden importer");

    r.write(path, &manifest("6.0.0"));
    let stale = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(!stale.status.success(), "{}", report(&stale));
    lock["importers"][id] = importer("7.0.0");
    lock["packages"]["is-number@7.0.0"] = json!({"resolution":{}});
    lock["snapshots"]["is-number@7.0.0"] = json!({});
    r.write("pnpm-lock.yaml", &lock.to_string());
    let edit = r.cmd(CLI, &["converge", "HEAD"]);

    fs::remove_file(r.0.path().join(path)).unwrap();
    lock["importers"].as_object_mut().unwrap().remove(id);
    r.write("pnpm-lock.yaml", &lock.to_string());
    let remove = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(
        edit.status.success() && remove.status.success(),
        "edit: {}\nremove: {}",
        report(&edit),
        report(&remove)
    );
    assert_eq!(graph(&edit.stdout)["importers"][id], importer("7.0.0"));
    assert!(graph(&remove.stdout)["importers"].get(id).is_none());

    // A rule and an authored specifier can change while the locked importer stays identical.
    r.write(path, &manifest("6.0.0"));
    lock["importers"][id] = importer("6.0.0");
    lock["overrides"] = json!({"is-number@5.0.0":"6.0.0"});
    r.write(
        "pnpm-workspace.yaml",
        &json!({"packages":["apps/*"],"overrides":lock["overrides"]}).to_string(),
    );
    r.write("pnpm-lock.yaml", &lock.to_string());
    let unchanged = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(unchanged.status.success(), "{}", report(&unchanged));
    assert_eq!(graph(&unchanged.stdout)["importers"][id], importer("6.0.0"));

    // The new extension affects only the release the manifest just stopped using.
    lock["overrides"] = overrides;
    let settings = json!({"packages":["apps/*"],"overrides":lock["overrides"],
        "packageExtensions":{"is-number@6":{"dependencies":{"is-buffer":"2.0.5"}}}});
    r.write("pnpm-workspace.yaml", &settings.to_string());
    // Recorded by pnpm 10.34.9 for the extension above.
    lock["packageExtensionsChecksum"] =
        json!("sha256-zWdDqa44lCRfRipKQcYOcX1RGy6x468mGsfynbmxO5k=");
    lock["importers"][id] = importer("7.0.0");
    for section in ["packages", "snapshots"] {
        lock[section]
            .as_object_mut()
            .unwrap()
            .remove("is-number@6.0.0");
    }
    r.write("pnpm-lock.yaml", &lock.to_string());
    let replaced = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(replaced.status.success(), "{}", report(&replaced));
    assert_eq!(graph(&replaced.stdout)["importers"][id], importer("7.0.0"));
}

#[test]
fn converge_keeps_a_generated_members_peer_pin_across_an_unrelated_edit() {
    let r = Repo::new();
    r.write(".gitignore", "apps/generated/\n");
    let id = "apps/generated";
    let mut lock = graph(&fs::read(r.0.path().join("pnpm-lock.yaml")).unwrap());
    for (range, pin, link, expected) in [
        ("^1", "1.0.0", "link:../before", "1.0.0"),
        ("^1", "1.1.0", "link:../after", "1.0.0"),
        (">=1", "1.1.0", "link:../after", "1.1.0"),
    ] {
        r.write(
            "apps/generated/package.json",
            &json!({"peerDependencies":{"foo":range},"dependencies":{"local":link}}).to_string(),
        );
        lock["importers"][id] = json!({"dependencies":{"foo":{"specifier":range,"version":pin},"local":{"specifier":link,"version":link}}});
        let key = format!("foo@{pin}");
        lock["packages"][&key] = json!({"resolution":{}});
        lock["snapshots"][&key] = json!({});
        r.write("pnpm-lock.yaml", &lock.to_string());
        if pin == "1.0.0" {
            r.commit("record the generated member's peer");
            continue;
        }
        let result = r.cmd(CLI, &["converge", "HEAD"]);
        assert!(result.status.success(), "{}", report(&result));
        let doc = graph(&result.stdout);
        assert_eq!(
            doc["importers"][id]["dependencies"]["foo"]["version"], expected,
            "{range}"
        );
        assert_eq!(
            doc["importers"][id]["dependencies"]["local"]["version"],
            link
        );
    }
}

#[test]
fn converge_accepts_an_ignored_members_dependency_edit_with_unchanged_dev_engines() {
    let r = Repo::new();
    r.write(".gitignore", "apps/generated/\n");
    let mut lock = graph(&fs::read(r.0.path().join("pnpm-lock.yaml")).unwrap());
    let mut member = json!({"devEngines":{"packageManager":{"name":"pnpm","version":"10.34.9"}}});
    for spec in ["link:../original", "link:../edited"] {
        member["dependencies"]["local"] = json!(spec);
        lock["importers"]["apps/generated"]["dependencies"]["local"] =
            json!({"specifier":spec,"version":spec});
        r.write("apps/generated/package.json", &member.to_string());
        r.write("pnpm-lock.yaml", &lock.to_string());
        if spec == "link:../original" {
            r.commit("record generated importer");
        }
    }
    let output = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(output.status.success(), "{}", report(&output));
    assert_eq!(
        graph(&output.stdout)["importers"]["apps/generated"],
        lock["importers"]["apps/generated"]
    );
}

#[test]
fn converge_ignores_changes_to_inactive_package_json_settings() {
    let r = Repo::new();
    let mut root = json!({"packageManager":"pnpm@12.21.0"});
    r.write("package.json", &root.to_string());
    r.commit("use pnpm 12");
    root["pnpm"] = json!({"ignoredBuiltDependencies":["build-tool"]});
    r.write("package.json", &root.to_string());
    let result = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(result.status.success(), "{}", report(&result));
    assert_eq!(
        result.stdout,
        fs::read(r.0.path().join("pnpm-lock.yaml")).unwrap()
    );
}

#[test]
fn convergence_uses_the_bootstrap_version_for_unpinned_workspace_configuration() {
    let r = Repo::new();
    let raw = fs::read_to_string(r.0.path().join("pnpm-lock.yaml")).unwrap();
    let input = with_recorded_manager("12.21.0", &raw);
    r.write("pnpm-lock.yaml", &input);
    r.write("package.json", r#"{"pnpm":"unused by this version"}"#);
    r.commit("lock the package manager");
    check_workspace(r.0.path(), 0, "bootstrap version");
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, input.as_bytes());
}

#[test]
fn workspace_commands_preserve_an_ignored_optional_dependency_even_when_another_member_uses_it() {
    let r = Repo::new();
    let root = json!({"packageManager":"pnpm@10.34.9","optionalDependencies":{"shared":"1.0.0"}});
    let member = json!({"dependencies":{"shared":"1.0.0"}});
    let settings = json!({"packages":["apps/*"],"ignoredOptionalDependencies":["shared"]});
    let lock = json!({"lockfileVersion":"9.0","ignoredOptionalDependencies":["shared"],
        "importers":{".":{},"apps/web":{"dependencies":{"shared":{"specifier":"1.0.0","version":"1.0.0"}}}},
        "packages":{"shared@1.0.0":{"resolution":{}}},"snapshots":{"shared@1.0.0":{}}}).to_string();
    r.write("package.json", &root.to_string());
    r.write("apps/web/package.json", &member.to_string());
    r.write("pnpm-workspace.yaml", &settings.to_string());
    r.write("pnpm-lock.yaml", &lock);
    r.write(
        "context.json",
        &json!({"target":"ours","manifests":{".":root,"apps/web":member},
        "configuration":settings,"configurations":[settings,settings,settings],"patches":{}})
        .to_string(),
    );
    r.commit("installed workspace");
    check_workspace(r.0.path(), 0, "installed lock");
    let mut failures = Vec::new();
    for args in [
        &["converge", "HEAD"][..],
        &["repair", "HEAD"],
        &[
            "merge",
            "pnpm-lock.yaml",
            "pnpm-lock.yaml",
            "pnpm-lock.yaml",
            "--context",
            "context.json",
        ],
    ] {
        r.write("pnpm-lock.yaml", &lock);
        let out = r.cmd(CLI, args);
        if !out.status.success() {
            failures.push(format!("{args:?}: {}", report(&out)));
            continue;
        }
        r.write("pnpm-lock.yaml", std::str::from_utf8(&out.stdout).unwrap());
        let check = r.cmd(CLI, &["validate", "--workspace"]);
        if !check.status.success() {
            failures.push(format!("{args:?}: {}", report(&check)));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn writing_mixed_optional_exclusions_preserves_the_installers_choices() {
    let r = Repo::new();
    let settings =
        json!({"packages":[],"pmOnFail":"ignore","ignoredOptionalDependencies":["*","!keep"]});
    let mut failures = Vec::new();
    for (version, recorded, filters_on_install, frozen) in [
        ("10.34.9", json!(["!keep", "*"]), true, 0),
        ("11.21.0", json!(["!keep", "*"]), true, 1),
        ("12.4.0", json!(["*", "!keep"]), false, 1),
        ("12.4.1", json!(["*", "!keep"]), true, 0),
    ] {
        let manifest = json!({"packageManager":format!("pnpm@{version}"),
            "optionalDependencies":{"keep":"1.0.0","dev":"2.0.0"},"devDependencies":{"dev":"1.0.0"}});
        let pin = json!({"specifier":"1.0.0","version":"1.0.0"});
        let (importer, dev_key, dev_snapshot) = if filters_on_install {
            (
                json!({"optionalDependencies":{"keep":pin},"devDependencies":{"dev":pin}}),
                "dev@1.0.0",
                json!({}),
            )
        } else {
            (
                json!({"optionalDependencies":{"keep":pin,"dev":{"specifier":"2.0.0","version":"2.0.0"}}}),
                "dev@2.0.0",
                json!({"optional":true}),
            )
        };
        let lock = json!({"lockfileVersion":"9.0","ignoredOptionalDependencies":recorded,
            "importers":{".": importer},"packages":{"keep@1.0.0":{"resolution":{}},dev_key:{"resolution":{}}},
            "snapshots":{"keep@1.0.0":{"optional":true},dev_key:dev_snapshot}});
        r.write("package.json", &manifest.to_string());
        r.write("pnpm-workspace.yaml", &settings.to_string());
        r.write("pnpm-lock.yaml", &lock.to_string());
        r.write(
            "context.json",
            &json!({"target":"ours","manifests":{".":manifest},"configuration":settings,
            "configurations":[settings,settings,settings],"patches":{}})
            .to_string(),
        );
        r.commit("record the installed graph");
        check_workspace(r.0.path(), frozen, version);
        for args in [
            &["converge", "HEAD"][..],
            &["repair", "HEAD"],
            &[
                "merge",
                "pnpm-lock.yaml",
                "pnpm-lock.yaml",
                "pnpm-lock.yaml",
                "--context",
                "context.json",
            ],
        ] {
            let out = r.cmd(CLI, args);
            if !out.status.success() {
                failures.push(format!("{version} {args:?}: {}", report(&out)));
            } else if graph(&out.stdout)["importers"]["."] != importer {
                failures.push(format!(
                    "{version} {args:?}: {}",
                    graph(&out.stdout)["importers"]["."]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn validate_uses_the_effective_project_scoped_optional_peers_setting() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(
        root,
        "package.json",
        json!({"packageManager":"pnpm@12.21.0"}),
    );
    for (configured, dedupe, recorded, status) in [
        (false, true, true, 1),
        (true, true, false, 1),
        (true, true, true, 0),
        (true, false, false, 0),
        (true, false, true, 1),
    ] {
        write_input(
            root,
            "pnpm-workspace.yaml",
            json!({"packages":[],"pmOnFail":"ignore",
            "projectScopedOptionalPeers":configured,"dedupePeerDependents":dedupe}),
        );
        let mut lock = json!({"lockfileVersion":"9.0","settings":{},"importers":{".":{}}});
        if recorded {
            lock["settings"]["projectScopedOptionalPeers"] = json!(true);
        }
        write_input(root, "pnpm-lock.yaml", lock);
        check_workspace(root, status, (configured, dedupe, recorded));
    }
}

#[test]
fn validate_ignores_a_newer_peer_header_on_versions_that_do_not_check_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(
        root,
        "pnpm-workspace.yaml",
        "packages: []\npmOnFail: ignore",
    );
    write_input(
        root,
        "pnpm-lock.yaml",
        json!({"lockfileVersion":"9.0",
        "settings":{"projectScopedOptionalPeers":true},"importers":{".":{}}}),
    );
    for (version, status) in [
        ("9.15.9", 0),
        ("10.34.9", 0),
        ("11.28.12", 0),
        ("12.18.0", 0),
        ("12.19.0", 1),
    ] {
        write_input(
            root,
            "package.json",
            json!({"packageManager":format!("pnpm@{version}")}),
        );
        check_workspace(root, status, version);
    }
}

#[test]
fn package_json_overrides_apply_only_to_the_pnpm_versions_that_read_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(
        root,
        "pnpm-workspace.yaml",
        "packages: []\npmOnFail: ignore",
    );
    for (version, legacy) in [
        ("9.15.9", true),
        ("10.34.9", true),
        ("11.28.12", false),
        ("12.21.0", false),
    ] {
        let rules = json!({"lib":"5.0.0"});
        write_input(
            root,
            "package.json",
            json!({"packageManager":format!("pnpm@{version}"),"pnpm":{"overrides":rules}}),
        );
        let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{}}});
        if legacy {
            lock["overrides"] = rules;
        }
        write_input(root, "pnpm-lock.yaml", lock);
        check_workspace(root, 0, version);
    }
}

#[track_caller]
fn check_workspace(root: &Path, expected: i32, case: impl std::fmt::Debug) -> Output {
    let result = Command::new(CLI)
        .current_dir(root)
        .args(["validate", "--workspace"])
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(expected),
        "{case:?}: {}",
        report(&result)
    );
    result
}

#[test]
fn inspect_reads_the_lockfiles_own_workspace_without_git_or_writes() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir = sandbox.path().join("nested");
    fs::create_dir_all(dir.join("packages/host")).unwrap();
    fs::write(
        dir.join("package.json"),
        r#"{"packageManager":"pnpm@10.34.9"}"#,
    )
    .unwrap();
    fs::write(
        dir.join("packages/host/package.json"),
        r#"{"name":"host","version":"2.0.0"}"#,
    )
    .unwrap();
    let input = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"plugin":{"specifier":"1","version":"1.0.0"}}},"packages/host":{}},"packages":{"plugin@1.0.0":{"resolution":{},"peerDependencies":{"host":"^1"}}},"snapshots":{"plugin@1.0.0":{"dependencies":{"host":"link:packages/host"}}}}).to_string();
    let path = dir.join("pnpm-lock.yaml");
    fs::write(&path, &input).unwrap();
    let inspect = || {
        Command::new(CLI)
            .current_dir(sandbox.path())
            .args(["inspect", "--json", "nested/pnpm-lock.yaml"])
            .output()
            .unwrap()
    };
    let ok = inspect();
    assert!(ok.status.success(), "{}", report(&ok));
    let result: Value = serde_json::from_slice(&ok.stdout).unwrap();
    assert_eq!(result["peerPolicy"], "pnpm10");
    let finding = &result["peers"][0];
    assert_eq!(finding["resolvedVersion"], "2.0.0");
    assert_eq!(finding["status"], "outOfRange");
    assert_eq!(finding["examplePath"], json!([".", "plugin@1.0.0"]));
    assert_eq!(fs::read_to_string(&path).unwrap(), input);
    fs::write(&path, "lockfileVersion: '9.0'\nsnapshots: []\n").unwrap();
    let invalid = inspect();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("snapshots"));
}

#[test]
fn an_unpinned_strict_merge_accepts_only_the_configured_peer_exception() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = json!({"dependencies":{"plugin":"1.0.0","host":"2.0.0"}});
    let raw = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"plugin":{"specifier":"1.0.0","version":"1.0.0(host@2.0.0)"},"host":{"specifier":"2.0.0","version":"2.0.0"}}}},"packages":{"plugin@1.0.0":{"resolution":{},"peerDependencies":{"host":"^1"}},"host@2.0.0":{"resolution":{}}},"snapshots":{"plugin@1.0.0(host@2.0.0)":{"dependencies":{"host":"2.0.0"}},"host@2.0.0":{}}});
    fs::write(dir.path().join("pnpm-lock.yaml"), raw.to_string()).unwrap();
    for allowed in [true, false] {
        let mut cfg = json!({"strictPeerDependencies":true});
        if allowed {
            cfg["peerDependencyRules"] = json!({"allowAny":["host"]});
        }
        let ctx = json!({"target":"ours","configuration":cfg,"configurations":[cfg,cfg,cfg],"patches":{},"manifests":{".":manifest}});
        fs::write(dir.path().join("ctx.json"), ctx.to_string()).unwrap();
        let out = Command::new(CLI)
            .current_dir(dir.path())
            .args([
                "merge",
                "pnpm-lock.yaml",
                "pnpm-lock.yaml",
                "pnpm-lock.yaml",
                "--context",
                "ctx.json",
            ])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(if allowed { 0 } else { 1 }),
            "{}",
            report(&out)
        );
        if !allowed {
            assert!(String::from_utf8_lossy(&out.stderr).contains("peer"));
        }
    }
    fs::write(
        dir.path().join("pnpm-workspace.yaml"),
        "peerDependencyRules: { allowAny: [host] }\n",
    )
    .unwrap();
    let out = Command::new(CLI)
        .current_dir(dir.path())
        .args(["inspect", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", report(&out));
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["peerPolicy"], "unspecified");
    assert_eq!(r["peers"][0]["disposition"], "unknown");
}

#[test]
fn inspection_reads_versions_only_from_manifests_inside_the_selected_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let shared = temp.path().join("shared");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&shared).unwrap();
    fs::write(
        shared.join("package.json"),
        r#"{"name":"host","version":"1.4.0"}"#,
    )
    .unwrap();
    let check = |provider: &str| {
        let pin = format!("link:{provider}");
        let raw = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"plugin":{"specifier":"1","version":"1.0.0"}}},provider:{}},
            "packages":{"plugin@1.0.0":{"resolution":{},"peerDependencies":{"host":"^1"}}},"snapshots":{"plugin@1.0.0":{"dependencies":{"host":pin}}}});
        fs::write(root.join("pnpm-lock.yaml"), raw.to_string()).unwrap();
        let out = Command::new(CLI)
            .current_dir(&root)
            .args(["inspect", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", report(&out));
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        let p = &result["peers"][0];
        assert_eq!(p["reason"], "providerVersion");
        assert!(p["resolvedVersion"].is_null());
        assert_eq!(p["bindings"][0]["reference"], pin);
    };
    check("../shared");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&shared, root.join("linked")).unwrap();
        check("linked");
        fs::create_dir(root.join("member")).unwrap();
        std::os::unix::fs::symlink(
            shared.join("package.json"),
            root.join("member/package.json"),
        )
        .unwrap();
        check("member");
    }
}

#[cfg(unix)]
#[test]
fn repair_reads_the_committed_target_of_a_workspace_directory_link() {
    let r = Repo::new();
    r.write(
        "sources/tool/package.json",
        r#"{"name":"tool","dependencies":{"leaf":"link:../../leaf"}}"#,
    );
    let mut doc = graph(r.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes());
    for (id, target) in [
        ("apps/tool", "../sources/tool"),
        ("apps/alias", "./../sources/./tool"),
    ] {
        std::os::unix::fs::symlink(target, r.0.path().join(id)).unwrap();
        doc["importers"][id] = json!({"dependencies":{"leaf":{"specifier":"link:../../leaf","version":"link:../../leaf"}}});
    }
    std::os::unix::fs::symlink("./../apps", r.0.path().join("apps/ancestor")).unwrap();
    std::os::unix::fs::symlink("./self", r.0.path().join("apps/self")).unwrap();
    let source = doc.to_string();
    r.write("pnpm-lock.yaml", &source);
    r.commit("include linked workspace");
    fs::remove_file(r.0.path().join("apps/tool")).unwrap();
    fs::remove_file(r.0.path().join("apps/alias")).unwrap();
    r.write("sources/tool/package.json", "{}");
    let out = r.cmd(CLI, &["repair", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(out.stdout, source.as_bytes());
    std::os::unix::fs::symlink("../../outside", r.0.path().join("apps/escape")).unwrap();
    r.commit("record an external workspace link");
    let outside = r.cmd(CLI, &["repair", "HEAD"]);
    assert!(!outside.status.success());
    assert!(
        report(&outside).contains("leaves the committed Git tree"),
        "{}",
        report(&outside)
    );
}

#[cfg(unix)]
#[test]
fn only_non_utf8_conflicts_reached_through_a_manifest_link_block_the_driver() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    for linked in [true, false] {
        let r = Repo::new();
        // Construct the raw tree entries in Git's index. Some filesystems cannot
        // check out the target filename even though Git can merge its revisions.
        let stage = |mode: &str, path: &[u8], data: &[u8]| {
            fs::write(r.0.path().join("content"), data).unwrap();
            let id = r.git(&["hash-object", "-w", "content"]);
            let result = Command::new("git")
                .current_dir(r.0.path())
                .args(["update-index", "--add", "--cacheinfo", mode, id.trim()])
                .arg(OsStr::from_bytes(path))
                .output()
                .unwrap();
            assert!(result.status.success(), "{}", report(&result));
        };
        let target = b"tool-\xff.json";
        stage(
            "120000",
            b"apps/tool/package.json",
            if linked {
                b"../../tool-\xff.json"
            } else {
                b"../../package.json"
            },
        );
        let revision = |side: &str, parent: &str| {
            r.side(side);
            r.git(&["add", "apps/web/package.json", "pnpm-lock.yaml"]);
            if side == "theirs" {
                let out = Command::new("git")
                    .current_dir(r.0.path())
                    .args(["update-index", "--force-remove", "--"])
                    .arg(OsStr::from_bytes(target))
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{}", report(&out));
            } else {
                stage(
                    "100644",
                    target,
                    json!({"name":side}).to_string().as_bytes(),
                );
            }
            fs::copy(r.0.path().join("pnpm-lock.yaml"), r.0.path().join(side)).unwrap();
            let tree = r.git(&["write-tree"]);
            r.git(&["commit-tree", tree.trim(), "-p", parent, "-m", side])
                .trim()
                .to_owned()
        };
        let base = revision("base", "HEAD");
        let ours = revision("ours", &base);
        let theirs = revision("theirs", &base);
        r.git(&["update-ref", "refs/heads/main", &ours]);
        let out = Command::new(CLI)
            .current_dir(r.0.path())
            .env(format!("GITHEAD_{theirs}"), "theirs")
            .args([
                "merge",
                "--git-driver",
                "base",
                "ours",
                "theirs",
                "pnpm-lock.yaml",
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.success(), !linked, "{}", report(&out));
        if linked {
            assert!(
                report(&out).contains("resolve dependency configuration first"),
                "{}",
                report(&out)
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn a_shadowed_workspace_manifest_is_never_followed() {
    let r = Repo::new();
    std::os::unix::fs::symlink(
        "/unused-manifest",
        r.0.path().join("apps/web/package.json5"),
    )
    .unwrap();
    r.commit("add a lower-priority manifest link");
    let out = r.cmd(CLI, &["repair", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));

    let r = Repo::new();
    r.write("outside/unused.json", "{}");
    std::os::unix::fs::symlink(
        "../../outside/unused.json",
        r.0.path().join("apps/web/package.json5"),
    )
    .unwrap();
    std::os::unix::fs::symlink("outside/unused.json", r.0.path().join("package.json5")).unwrap();
    r.commit("link the lower-priority manifest");
    r.git(&["branch", "-f", "topic", "HEAD"]);
    r.branches(|r, side| r.write("outside/unused.json", &json!({"side":side}).to_string()));
    let out = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(!out.status.success());
    assert_eq!(
        r.git(&["diff", "--name-only", "--diff-filter=U"]).trim(),
        "outside/unused.json",
        "{}",
        report(&out)
    );
}

#[cfg(unix)]
#[test]
fn a_lower_priority_manifest_name_can_also_link_a_nested_member_directory() {
    let r = Repo::new();
    r.write(
        "pnpm-workspace.yaml",
        "packages: ['apps/*', 'apps/web/package.json5/*', '!apps/[a/b]']\npmOnFail: ignore\n",
    );
    r.write(
        "sources/collection/nested/package.json",
        r#"{"name":"nested"}"#,
    );
    std::os::unix::fs::symlink(
        "../../sources/collection",
        r.0.path().join("apps/web/package.json5"),
    )
    .unwrap();
    r.commit("add nested workspace member");
    let out = r.cmd(CLI, &["repair", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert!(
        graph(&out.stdout)["importers"]
            .get("apps/web/package.json5/nested")
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn an_unresolved_member_directory_link_blocks_the_lock_even_if_the_preview_has_no_manifest() {
    let r = Repo::new();
    for path in ["old", "empty", "member"] {
        r.write(&format!("sources/{path}/readme"), "tracked");
    }
    r.write("sources/member/package.json", r#"{"name":"member"}"#);
    let link = r.0.path().join("apps/tool");
    std::os::unix::fs::symlink("../sources/old", &link).unwrap();
    r.commit("link to an empty member");
    r.git(&["branch", "-f", "topic", "HEAD"]);
    r.branches(|_, side| {
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(
            if side == "ours" {
                "../sources/empty"
            } else {
                "../sources/member"
            },
            &link,
        )
        .unwrap();
    });
    let out = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(!out.status.success());
    assert!(
        r.git(&["diff", "--name-only", "--diff-filter=U"])
            .lines()
            .any(|p| p == "pnpm-lock.yaml"),
        "{}",
        report(&out)
    );
}

#[cfg(unix)]
#[test]
fn an_untracked_link_through_a_regular_file_is_not_a_workspace_directory() {
    let repo = Repo::new();
    repo.write("file", "not a directory");
    std::os::unix::fs::symlink("../file/child", repo.0.path().join("apps/broken")).unwrap();
    for args in [["converge", "HEAD"], ["validate", "--workspace"]] {
        let out = repo.cmd(CLI, &args);
        assert!(out.status.success(), "{}", report(&out));
    }
}

#[cfg(unix)]
#[test]
fn converge_ignores_a_cyclic_directory_symlink_like_pnpm() {
    let r = Repo::new();
    std::os::unix::fs::symlink("self", r.0.path().join("apps/self")).unwrap();
    r.commit("add a symlink cycle");
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
}

#[cfg(unix)]
#[test]
fn a_conflicted_target_manifest_keeps_its_linked_workspaces_lock_unmerged() {
    let r = Repo::new();
    r.write("sources/tool/package.json", r#"{"name":"tool"}"#);
    std::os::unix::fs::symlink("../sources/tool", r.0.path().join("apps/tool")).unwrap();
    r.commit("link the workspace");
    r.git(&["branch", "-f", "topic", "HEAD"]);
    r.branches(|r, side| {
        if side == "ours" {
            r.write("sources/tool/package.json", r#"{"name":"renamed-tool"}"#);
        } else {
            fs::remove_file(r.0.path().join("sources/tool/package.json")).unwrap();
        }
        let mut doc = graph(&fs::read(r.0.path().join("pnpm-lock.yaml")).unwrap());
        if side == "ours" {
            doc["importers"]["apps/tool"] = json!({});
        }
        r.write("pnpm-lock.yaml", &doc.to_string());
    });
    let merge = r.cmd("git", &["merge", "--no-edit", "topic"]);
    assert!(!merge.status.success());
    assert!(
        r.git(&["diff", "--name-only", "--diff-filter=U"])
            .lines()
            .any(|p| p == "pnpm-lock.yaml"),
        "{}",
        report(&merge)
    );
    assert!(
        report(&merge).contains("sources/tool/package.json"),
        "{}",
        report(&merge)
    );
}

#[cfg(unix)]
#[test]
fn converge_checks_the_installed_importer_for_a_new_working_directory_link() {
    for assumed_unchanged in [false, true] {
        let r = Repo::new();
        r.write(
            "sources/tool/package.json",
            r#"{"name":"tool","dependencies":{"local":"link:../../local"}}"#,
        );
        if assumed_unchanged {
            r.write("apps/tool", "regular file in the index");
            r.commit("track an ordinary file");
            r.git(&["update-index", "--assume-unchanged", "apps/tool"]);
            fs::remove_file(r.0.path().join("apps/tool")).unwrap();
        }
        std::os::unix::fs::symlink("../sources/tool", r.0.path().join("apps/tool")).unwrap();
        let out = r.cmd(CLI, &["converge", "HEAD"]);
        assert_eq!(out.status.code(), Some(2), "{}", report(&out));
        assert!(report(&out).contains("apps/tool"));
        let mut doc = graph(r.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes());
        doc["importers"]["apps/tool"] = json!({"dependencies":{"local":{"specifier":"link:../../local","version":"link:../../local"}}});
        r.write("pnpm-lock.yaml", &doc.to_string());
        let out = r.cmd(CLI, &["converge", "HEAD"]);
        assert!(out.status.success(), "{}", report(&out));
        let importer = &graph(&out.stdout)["importers"]["apps/tool"];
        assert_eq!(
            importer["dependencies"]["local"]["version"],
            "link:../../local"
        );
    }
}

#[cfg(unix)]
#[test]
fn git_workspace_commands_ignore_unrelated_non_utf8_tree_entries() {
    use std::os::unix::ffi::OsStrExt;
    let r = Repo::new();
    let blob = r.git(&["rev-parse", "HEAD:package.json"]);
    let out = Command::new("git")
        .current_dir(r.0.path())
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            "100644",
            blob.trim(),
        ])
        .arg(std::ffi::OsStr::from_bytes(b"archive-\xff.bin"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", report(&out));
    r.git(&["commit", "-m", "track a binary asset"]);
    for operation in ["repair", "converge"] {
        let out = r.cmd(CLI, &[operation, "HEAD"]);
        assert!(out.status.success(), "{}", report(&out));
    }
}

#[test]
fn newly_declaring_a_runner_migrates_the_workspace_even_without_a_bootstrap_document() {
    let r = Repo::new();
    r.write("package.json", "{}");
    r.write("pnpm-workspace.yaml", "packages: ['apps/*']\n");
    r.commit("start with an unspecified pnpm version");
    r.write("package.json", r#"{"packageManager":"pnpm@10.34.9"}"#);
    r.write(
        "pnpm-workspace.yaml",
        "packages: ['apps/*']\npmOnFail: ignore\n",
    );
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(
        graph(&out.stdout),
        graph(r.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes())
    );
}

fn registry_command(root: &Path) -> Command {
    let mut command = Command::new(CLI);
    command
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

#[test]
fn workspace_validation_uses_the_effective_registry_for_implicit_config_tarballs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    write_input(
        root,
        "pnpm-workspace.yaml",
        "packages: []\nconfigDependencies:\n  tool:\n    integrity: 1.0.0+sha512-fixture\n    tarball: https://private.example/tool/-/tool-1.0.0.tgz\n",
    );
    let boot = json!({"lockfileVersion":"9.0","importers":{".":{"configDependencies":{"tool":{"specifier":"1.0.0","version":"1.0.0"}}}},
        "packages":{"tool@1.0.0":{"resolution":{"integrity":"sha512-fixture"}}},"snapshots":{"tool@1.0.0":{}}});
    write_input(
        root,
        "pnpm-lock.yaml",
        format!("---\n{boot}\n---\nlockfileVersion: '9.0'\nimporters: {{.: {{}}}}\n"),
    );
    for (registry, valid) in [
        ("https://private.example", true),
        ("https://other.example", false),
    ] {
        let out = registry_command(root)
            .env("pnpm_config_registry", registry)
            .args(["validate", "--workspace"])
            .output()
            .unwrap();
        assert_eq!(out.status.success(), valid, "{}", report(&out));
    }
    write_input(
        root,
        "global/pnpm/config.yaml",
        "registry: https://private.example\n",
    );
    let out = registry_command(root)
        .env("XDG_CONFIG_HOME", root.join("global"))
        .args(["validate", "--workspace"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", report(&out));

    let repo = Repo::new();
    repo.write("package.json", "{}");
    repo.write("pnpm-workspace.yaml", "packages: []\n");
    repo.write(
        "pnpm-lock.yaml",
        "lockfileVersion: '9.0'\nimporters: {.: {}}\n",
    );
    repo.commit("no config dependencies");
    for file in ["pnpm-workspace.yaml", "pnpm-lock.yaml"] {
        repo.write(file, &fs::read_to_string(root.join(file)).unwrap());
    }
    let converge = |host| {
        registry_command(repo.0.path())
            .env("pnpm_config_registry", host)
            .args(["converge", "HEAD"])
            .output()
            .unwrap()
    };
    let out = converge("https://private.example");
    assert!(out.status.success(), "{}", report(&out));
    let mismatch = converge("https://other.example");
    assert!(!mismatch.status.success());
    assert!(
        report(&mismatch).contains("bootstrap graph differs from the configuration dependencies"),
        "{}",
        report(&mismatch)
    );

    repo.commit("record the config dependency and implicit tarball");
    let unchanged = converge("https://private.example");
    assert!(unchanged.status.success(), "{}", report(&unchanged));
    assert_eq!(
        unchanged.stdout,
        fs::read(root.join("pnpm-lock.yaml")).unwrap()
    );

    let mut stale = boot.clone();
    stale["packages"]["tool@1.0.0"]["resolution"]["tarball"] =
        json!("https://other.example/tool/-/tool-1.0.0.tgz");
    repo.write(
        "pnpm-lock.yaml",
        &format!("---\n{stale}\n---\nlockfileVersion: '9.0'\nimporters: {{.: {{}}}}\n"),
    );
    repo.commit("record a stale bootstrap artifact");
    let candidate = fs::read_to_string(root.join("pnpm-lock.yaml")).unwrap();
    repo.write("pnpm-lock.yaml", &candidate);
    let converged = converge("https://private.example");
    assert!(converged.status.success(), "{}", report(&converged));
    let lock = Lockfile::try_from(String::from_utf8(converged.stdout).unwrap()).unwrap();
    assert_eq!(
        lock.environment(),
        Lockfile::parse(&candidate).unwrap().environment()
    );

    let cfg = fs::read_to_string(root.join("pnpm-workspace.yaml")).unwrap();
    repo.write(
        "pnpm-workspace.yaml",
        &format!("{cfg}\nregistries: {{default: 'https://other.example'}}\n"),
    );
    let changed = converge("https://private.example");
    assert!(!changed.status.success());
    assert!(
        report(&changed).contains("changed workspace setting registries"),
        "{}",
        report(&changed)
    );

    let mut explicit = boot.clone();
    for (url, valid) in [
        ("https://private.example/tool/-/tool-1.0.0.tgz", true),
        ("https://other.example/custom", false),
    ] {
        explicit["packages"]["tool@1.0.0"]["resolution"]["tarball"] = json!(url);
        write_input(
            root,
            "pnpm-lock.yaml",
            format!("---\n{explicit}\n---\nlockfileVersion: '9.0'\nimporters: {{.: {{}}}}\n"),
        );
        let out = registry_command(root)
            .env("npm_config_userconfig", root)
            .args(["validate", "--workspace"])
            .output()
            .unwrap();
        assert_eq!(out.status.success(), valid, "{url}: {}", report(&out));
        if !valid {
            assert!(
                report(&out).contains("configDependencies differ"),
                "{}",
                report(&out)
            );
        }
    }
}

#[test]
fn merge_can_select_an_explicit_bootstrap_when_another_candidate_needs_registry_settings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let url = "https://private.example/tool/-/tool-1.0.0.tgz";
    let settings = json!({
        "configDependencies": {"tool": {"integrity": "1.0.0+sha512-fixture", "tarball": url}}
    });
    let context = json!({
        "target": "ours",
        "manifests": {".": {}},
        "configurations": [settings, settings, settings],
        "configuration": settings,
        "patches": {}
    });
    write_input(root, "ctx", context.to_string());
    let mut boot = json!({
        "lockfileVersion": "9.0",
        "importers": {".": {"configDependencies": {"tool": {"specifier": "1.0.0", "version": "1.0.0"}}}},
        "packages": {"tool@1.0.0": {"resolution": {"integrity": "sha512-fixture"}}},
        "snapshots": {"tool@1.0.0": {}}
    });
    let lock =
        |env: &Value| format!("---\n{env}\n---\nlockfileVersion: '9.0'\nimporters: {{.: {{}}}}\n");
    write_input(root, "implicit", lock(&boot));
    boot["packages"]["tool@1.0.0"]["resolution"]["tarball"] = json!(url);
    write_input(root, "explicit", lock(&boot));
    for (ours, theirs) in [("explicit", "implicit"), ("implicit", "explicit")] {
        let out = registry_command(root)
            .env("npm_config_userconfig", root)
            .args(["merge", "implicit", ours, theirs, "--context", "ctx"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", report(&out));
        let result = Lockfile::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
        assert_eq!(
            result.environment().unwrap()["packages"]["tool@1.0.0"]["resolution"]["tarball"],
            url
        );
    }

    boot["packages"]["tool@1.0.0"]["resolution"]["tarball"] =
        json!("https://other.example/tool/-/tool-1.0.0.tgz");
    write_input(root, "wrong", lock(&boot));
    let out = registry_command(root)
        .env("npm_config_userconfig", root)
        .args(["merge", "implicit", "wrong", "wrong", "--context", "ctx"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
    assert!(out.stdout.is_empty());
    let diagnostic = report(&out);
    assert!(
        diagnostic.contains("no bootstrap graph matches"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains(root.to_str().unwrap()), "{diagnostic}");
}

#[test]
fn converge_accepts_a_pnpm_patch_release_upgrade() {
    let r = Repo::new();
    r.write("package.json", r#"{"packageManager":"pnpm@10.34.8"}"#);
    r.commit("pin package manager");
    r.write("package.json", r#"{"packageManager":"pnpm@10.34.9"}"#);
    let out = r.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    assert_eq!(
        graph(&out.stdout),
        graph(r.git(&["show", "HEAD:pnpm-lock.yaml"]).as_bytes())
    );
}

#[test]
fn converge_replaces_an_invalid_baseline_manager_pin_even_when_the_version_is_unchanged() {
    let repo = Repo::new();
    let project = repo.git(&["show", "HEAD:pnpm-lock.yaml"]);
    let candidate = with_recorded_manager("12.21.0", &project);
    repo.write("package.json", r#"{"packageManager":"pnpm@12.21.0"}"#);
    repo.write("pnpm-workspace.yaml", "packages: ['apps/*']\n");
    repo.write(
        "pnpm-lock.yaml",
        &candidate.replace(r#""specifier":"12.21.0""#, r#""specifier":"12.20.0""#),
    );
    repo.commit("record the same pnpm version with a stale specifier");
    repo.write("pnpm-lock.yaml", &candidate);

    let out = repo.cmd(CLI, &["converge", "HEAD"]);
    assert!(out.status.success(), "{}", report(&out));
    let merged = Lockfile::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
    assert_eq!(
        merged.environment(),
        Lockfile::parse(&candidate).unwrap().environment()
    );
}

#[test]
fn inspect_reads_an_in_tree_peer_provider_without_an_importer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(
        root,
        "vendor/host/package.json",
        r#"{"name":"host","version":"1.0.0"}"#,
    );
    let doc = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"plugin":{"specifier":"1","version":"1.0.0"}}}},
        "packages":{"plugin@1.0.0":{"resolution":{},"peerDependencies":{"host":"^1.0.0"}}},
        "snapshots":{"plugin@1.0.0":{"dependencies":{"host":"link:vendor/host"}}}});
    write_input(root, "pnpm-lock.yaml", doc.to_string());
    let inspect = || {
        let out = Command::new(CLI)
            .current_dir(root)
            .args(["inspect", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", report(&out));
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    assert_eq!(inspect()["peers"], json!([]));
    fs::remove_dir_all(root.join("vendor")).unwrap();
    write_input(root, "vendor", "not a directory");
    let peers = inspect();
    assert_eq!(peers["peers"][0]["status"], "unknown");
    assert_eq!(peers["peers"][0]["reason"], "providerVersion");
}

#[test]
fn workspace_validation_distinguishes_invalid_and_quoted_lock_settings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_input(root, "package.json", "{}");
    for (settings, workspace, valid) in [
        (json!("default"), json!({}), false),
        (json!({"autoInstallPeers":"fales"}), json!({}), false),
        (json!({"autoInstallPeers":"false"}), json!({}), false),
        (
            json!({"autoInstallPeers":"false"}),
            json!({"autoInstallPeers":false}),
            true,
        ),
    ] {
        write_input(
            root,
            "pnpm-lock.yaml",
            json!({"lockfileVersion":"9.0","settings":settings,"importers":{".":{}}}).to_string(),
        );
        write_input(root, "pnpm-workspace.yaml", workspace.to_string());
        let out = Command::new(CLI)
            .current_dir(root)
            .args(["validate", "--workspace"])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(if valid { 0 } else { 1 }),
            "{settings}: {}",
            report(&out)
        );
    }
}
