use std::collections::BTreeMap;

use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeInput, MergeTarget, RepairInput, WorkspaceConfig,
    merge,
};
use serde_json::json;

const EMPTY: &str = "lockfileVersion: '9.0'\nimporters: {'.': {}}\n";

#[test]
fn bootstrap_metadata_is_available_from_a_single_yaml_document() {
    let yaml = "lockfileVersion: '9.0'\nimporters:\n  .:\n    packageManagerDependencies:\n      pnpm: {specifier: 12.21.0, version: 12.21.0}\n    configDependencies:\n      tool: {specifier: 1.0.0, version: 1.0.0}\npackages:\n  tool@1.0.0: {resolution: {integrity: sha512-example}}\n";
    let declarations = json!({"tool":{
        "integrity":"1.0.0+sha512-example","tarball":"https://registry.example/tool/-/tool-1.0.0.tgz"
    }});
    let declarations = declarations.as_object().unwrap();
    let version = "12.21.0".parse::<node_semver::Version>().unwrap();
    for text in [
        yaml.to_owned(),
        yaml.replace(
            "packageManagerDependencies",
            r#""packageManager\u0044ependencies""#,
        )
        .replace("configDependencies", r#""config\u0044ependencies""#),
    ] {
        let parsed = Lockfile::parse(&text).unwrap();
        assert_eq!(
            RepairInput::recorded_pnpm_version(&text),
            Some(version.clone())
        );
        for input in [MergeInput::from(text.as_str()), MergeInput::from(&parsed)] {
            assert_eq!(input.recorded_pnpm_version(), Some(version.clone()));
            assert_eq!(input.implicit_config_tarballs(declarations), ["tool"]);
        }
    }
    assert_eq!(RepairInput::recorded_pnpm_version(EMPTY), None);
    assert!(
        MergeInput::from(EMPTY)
            .implicit_config_tarballs(declarations)
            .is_empty()
    );
}

fn source(entries: &str) -> String {
    format!(
        "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      a: {{specifier: 1.0.0, version: 1.0.0}}\n      b: {{specifier: 1.0.0, version: 1.0.0}}\npackages:\n  a@1.0.0: {{resolution: {{integrity: sha512-a}}}}\n  b@1.0.0: {{resolution: {{integrity: sha512-b}}}}\n  child@1.0.0: {{resolution: {{integrity: sha512-child1}}}}\n  child@2.0.0: {{resolution: {{integrity: sha512-child2}}}}\n  child@3.0.0: {{resolution: {{integrity: sha512-child3}}}}\nsnapshots:\n{entries}  child@1.0.0: {{}}\n  child@2.0.0: {{}}\n  child@3.0.0: {{}}\n"
    )
}

fn recover(text: &str, side: usize, overrides: bool) -> Result<Lockfile, LockfileError> {
    let old = WorkspaceConfig::default();
    let final_config = WorkspaceConfig::from_pnpm(&if overrides {
        json!({"overrides":{"child":"3.0.0"}})
    } else {
        json!({})
    })
    .unwrap();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"1.0.0","b":"1.0.0"}}),
    )]);
    let mut inputs = [EMPTY; 3];
    inputs[side] = text;
    merge(
        inputs[0],
        inputs[1],
        inputs[2],
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&old, &old, &old],
            configuration: &final_config,
            patches: &BTreeMap::new(),
        }),
    )
}

#[test]
fn valid_yaml_uses_the_same_merge_and_text_preservation_contract() {
    let text = source("  a@1.0.0: {}\n  b@1.0.0: {}\n");
    let lock = Lockfile::parse(&text).unwrap();
    let config = WorkspaceConfig::default();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"1.0.0","b":"1.0.0"}}),
    )]);
    let context = MergeContext {
        registry: None,
        target: MergeTarget::Ours,
        manifests: &manifests,
        configurations: [&config; 3],
        configuration: &config,
        patches: &BTreeMap::new(),
    };
    let expected = merge(&lock, &lock, &lock, Some(&context)).unwrap();
    assert_eq!(
        merge(text.as_str(), text.as_str(), text.as_str(), Some(&context))
            .unwrap()
            .to_yaml()
            .unwrap(),
        expected.to_yaml().unwrap()
    );
    assert_eq!(
        merge(&lock, text.as_str(), &lock, Some(&context))
            .unwrap()
            .to_yaml()
            .unwrap(),
        expected.to_yaml().unwrap()
    );
}

#[test]
fn identical_snapshot_duplicates_are_removed_without_weakening_strict_parse() {
    let text = source(
        "  a@1.0.0: {dependencies: {child: 1.0.0}}\n  a@1.0.0: {dependencies: {child: 1.0.0}}\n  b@1.0.0: {}\n",
    );
    assert!(matches!(
        Lockfile::parse(&text),
        Err(LockfileError::Parse(_))
    ));
    assert!(matches!(
        merge(EMPTY, text.as_str(), EMPTY, None),
        Err(LockfileError::Parse(_))
    ));
    for side in 0..3 {
        let result = recover(&text, side, false).unwrap();
        assert_eq!(
            result.project().unwrap()["snapshots"]["a@1.0.0"]["dependencies"]["child"],
            "1.0.0"
        );
        Lockfile::parse(&result.to_yaml().unwrap())
            .unwrap()
            .validate()
            .unwrap();
    }
}

#[test]
fn independent_duplicate_snapshots_converge_under_final_overrides() {
    let text = source(
        "  a@1.0.0: {dependencies: {child: 1.0.0}}\n  a@1.0.0: {dependencies: {child: 2.0.0}}\n  b@1.0.0: {dependencies: {child: 2.0.0}}\n  b@1.0.0: {dependencies: {child: 1.0.0}}\n",
    );
    for side in 0..3 {
        let result = recover(&text, side, true).unwrap();
        for parent in ["a@1.0.0", "b@1.0.0"] {
            assert_eq!(
                result.project().unwrap()["snapshots"][parent]["dependencies"]["child"],
                "3.0.0"
            );
        }
        Lockfile::parse(&result.to_yaml().unwrap())
            .unwrap()
            .validate()
            .unwrap();
    }
}

#[test]
fn matching_first_and_last_interpretations_do_not_hide_a_different_middle() {
    let text = source(
        "  a@1.0.0: {dependencies: {child: 1.0.0}}\n  a@1.0.0: {dependencies: {child: 1.0.0}, custom: changed}\n  a@1.0.0: {dependencies: {child: 2.0.0}}\n  b@1.0.0: {}\n",
    );
    assert!(
        matches!(recover(&text, 1, true), Err(LockfileError::ResolutionRequired { message, .. }) if message.contains("different graphs"))
    );
}

#[test]
fn a_failed_interpretation_is_not_silently_discarded() {
    for body in [
        "{dependencies: {missing: 9.0.0}}",
        "null",
        "[]",
        "{dependencies: wrong}",
        "{dependencies: {child: 42}}",
    ] {
        let text = source(&format!(
            "  a@1.0.0: {{dependencies: {{child: 1.0.0}}}}\n  a@1.0.0: {body}\n  b@1.0.0: {{}}\n"
        ));
        assert!(
            matches!(recover(&text, 1, false), Err(LockfileError::ResolutionRequired { message, .. }) if message.contains("interpretation failed")),
            "{body}"
        );
    }
}

#[test]
fn duplicate_recovery_reports_work_budget_exhaustion_instead_of_sampling() {
    for pairs in [4, 5] {
        let mut entries = String::from("  a@1.0.0: {}\n  b@1.0.0: {}\n");
        // Four independent pairs fit the documented 16-combination budget;
        // five pairs exceed it, even when the records are unreachable.
        for index in 0..pairs {
            entries.push_str(&format!("  unused-{index}@1.0.0: {{custom: first}}\n  unused-{index}@1.0.0: {{custom: second}}\n"));
        }
        let result = recover(&source(&entries), 1, false);
        if pairs == 4 {
            result.unwrap().validate().unwrap();
        } else {
            assert!(
                matches!(result, Err(LockfileError::ResolutionRequired { message, .. }) if message.contains("work budget"))
            );
        }
    }
}

#[test]
fn recovery_rejects_duplicates_outside_snapshot_entries_and_merge_keys() {
    for text in [
        source("  a@1.0.0: {}\n  a@1.0.0: {}\n  b@1.0.0: {}\n")
            .replace("packages:", "custom: {0xB: first, 11: second}\npackages:"),
        source("  a@1.0.0: {dependencies: {child: 1.0.0, child: 2.0.0}}\n  b@1.0.0: {}\n"),
        source("  a@1.0.0: {}\n  b@1.0.0: {}\n").replace(
            "packages:",
            "settings: {autoInstallPeers: true, autoInstallPeers: false}\npackages:",
        ),
        source("  a@1.0.0: {}\n  a@1.0.0: {}\n  b@1.0.0: {}\n").replace(
            "packages:",
            "custom: &meta {key: value}\nextra: {<<: *meta}\npackages:",
        ),
        format!(
            "---\nnull\n---\n{}",
            source("  a@1.0.0: {}\n  a@1.0.0: {}\n  b@1.0.0: {}\n")
        ),
    ] {
        assert!(
            matches!(recover(&text, 1, false), Err(LockfileError::Parse(_))),
            "{text}"
        );
    }
}

#[test]
fn recovery_preserves_environment_documents_and_accepts_flow_snapshots() {
    let text = source("  a@1.0.0: {}\n  a@1.0.0: {custom: ignored}\n  b@1.0.0: {}\n");
    let text = format!(
        "---\nlockfileVersion: '9.0'\nimporters: {{'.': {{configDependencies: {{}}}}}}\n---\n{text}"
    );
    // Both alternatives become unreachable when final manifests remove a.
    let config = WorkspaceConfig::default();
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"b":"1.0.0"}}))]);
    let result = merge(
        EMPTY,
        text.as_str(),
        EMPTY,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    assert!(result.environment().is_some());
    assert!(
        result.project().unwrap()["snapshots"]
            .get("a@1.0.0")
            .is_none()
    );
    Lockfile::parse(&result.to_yaml().unwrap())
        .unwrap()
        .validate()
        .unwrap();
    let flow = source("  a@1.0.0: {}\n  b@1.0.0: {}\n");
    let flow = flow[..flow.find("snapshots:").unwrap()].to_owned()
        + "snapshots: {'a@1.0.0': {}, 'a@1.0.0': {}, 'b@1.0.0': {}}\n";
    recover(&flow, 1, false).unwrap().validate().unwrap();
}
