use locksmith::{ConflictKind, DocumentKind, Lockfile, LockfileError, Value, merge};
use serde_json::json;

fn lock(value: Value) -> Lockfile {
    Lockfile::parse(&serde_json::to_string(&value).unwrap()).unwrap()
}

fn graph(importers: Value, snapshots: Value) -> Value {
    let mut packages = serde_json::Map::new();
    for key in snapshots.as_object().unwrap().keys() {
        packages.insert(
            key.split('(').next().unwrap().into(),
            json!({"resolution": {"integrity": "sha512-test"}}),
        );
    }
    json!({"lockfileVersion":"9.0", "importers":importers, "packages": packages, "snapshots":snapshots})
}

fn dep(version: &str) -> Value {
    json!({"specifier":version.split('(').next().unwrap(),"version":version})
}

fn conflicts(error: LockfileError) -> Vec<locksmith::Conflict> {
    match error {
        LockfileError::Conflicts(conflicts) => conflicts,
        other => panic!("expected conflicts, got {other}"),
    }
}

#[test]
fn rejects_duplicate_keys_within_a_mapping_but_allows_same_package_in_both_sections() {
    let valid = "lockfileVersion: '9.0'\npackages:\n  ws@8.22.2:\n    resolution: {integrity: sha512-test}\nsnapshots:\n  ws@8.22.2: {}\n";
    Lockfile::parse(valid).unwrap().validate().unwrap();
    let broken = valid.replace("snapshots:", "  ws@8.22.2: {}\nsnapshots:");
    assert!(matches!(
        Lockfile::parse(&broken),
        Err(LockfileError::Parse(_))
    ));
    assert!(matches!(
        Lockfile::parse("lockfileVersion: '9.0'\nsettings: {a: 1, a: 2}"),
        Err(LockfileError::Parse(_))
    ));
}

#[test]
fn rejects_invalid_yaml_and_experimental_or_legacy_formats() {
    for text in [
        "",
        "null",
        "[]",
        "lockfileVersion: [",
        "importers: {}",
        "lockfileVersion: '9.0'\nimporters: null",
    ] {
        assert!(Lockfile::parse(text).is_err(), "accepted {text:?}");
    }
    for version in ["6.0", "9.1", "10.0", "12.0", "13.0"] {
        assert!(matches!(
            Lockfile::parse(&format!("lockfileVersion: '{version}'")),
            Err(LockfileError::UnsupportedVersion { .. })
        ));
    }
    Lockfile::parse("lockfileVersion: 9.0\nimporters: {}").unwrap();
}

#[test]
fn unchanged_bytes_include_comments_bom_and_crlf() {
    let text = "\u{feff}# generated\r\nlockfileVersion: '9.0'\r\n\r\nimporters:\r\n  .: {}\r\n";
    let parsed = Lockfile::parse(text).unwrap();
    assert_eq!(parsed.to_yaml().unwrap(), text);
    assert_eq!(
        merge(&parsed, &parsed, &parsed, None)
            .unwrap()
            .to_yaml()
            .unwrap(),
        text
    );
}

#[test]
fn merging_a_catalog_upgrade_with_a_new_consumer_rejects_the_stale_pin() {
    // Each branch is valid on its own. A new consumer from the other branch
    // must not retain the old pin after the workspace catalog is upgraded.
    let make = |version: &str, include_new_consumer: bool| {
        let dependency = json!({"specifier":"catalog:","version":version});
        let mut importers = json!({"app":{"dependencies":{"runtime":dependency}}});
        if include_new_consumer {
            importers["new-app"] = json!({"dependencies":{"runtime":dependency}});
        }
        let mut value = graph(importers, json!({format!("runtime@{version}"): {}}));
        value["catalogs"] = json!({"default":{"runtime":{"specifier":version,"version":version}}});
        let result = lock(value);
        result.validate().unwrap();
        result
    };
    let base = make("1.0.0", false);
    let upgraded = make("2.0.0", false);
    let added_consumer = make("1.0.0", true);
    for (ours, theirs) in [(&upgraded, &added_consumer), (&added_consumer, &upgraded)] {
        let Err(LockfileError::InvalidGraph(issues)) = merge(&base, ours, theirs, None) else {
            panic!("a stale catalog pin must prevent a successful merge");
        };
        assert!(
            issues
                .iter()
                .any(|issue| { issue.path == ["importers", "new-app", "dependencies", "runtime"] })
        );
    }
}

#[test]
fn merging_an_override_upgrade_with_a_new_consumer_rejects_the_stale_specifier() {
    let make = |version: &str, include_new_consumer: bool| {
        let dependency = dep(version);
        let mut importers = json!({"app":{"dependencies":{"runtime":dependency}}});
        if include_new_consumer {
            importers["new-app"] = json!({"dependencies":{"runtime":dependency}});
        }
        let mut value = graph(importers, json!({format!("runtime@{version}"): {}}));
        value["overrides"] = json!({"runtime":version});
        let result = lock(value);
        result.validate().unwrap();
        result
    };
    let base = make("1.0.0", false);
    let upgraded = make("2.0.0", false);
    let added_consumer = make("1.0.0", true);
    for (ours, theirs) in [(&upgraded, &added_consumer), (&added_consumer, &upgraded)] {
        let Err(LockfileError::InvalidGraph(issues)) = merge(&base, ours, theirs, None) else {
            panic!("a stale override specifier must prevent a successful merge");
        };
        assert!(
            issues
                .iter()
                .any(|issue| { issue.path == ["importers", "new-app", "dependencies", "runtime"] })
        );
    }
}

#[test]
fn patch_hashes_must_be_declared() {
    let mut donor = graph(
        json!({"app":{"dependencies":{"pkg":dep("1.0.0(patch_hash=abc)")}}}),
        json!({"pkg@1.0.0(patch_hash=abc)":{}}),
    );
    assert!(lock(donor.clone()).validate().is_err());
    for patch in [
        json!("abc"),
        json!({"hash":"abc","path":"patches/pkg.patch"}),
    ] {
        donor["patchedDependencies"] = json!({"pkg@1.0.0":patch});
        lock(donor.clone()).validate().unwrap();
    }
}

#[test]
fn missing_snapshot_package_or_version_is_an_error() {
    let baseline = graph(
        json!({"app":{"dependencies":{"pkg":dep("1.0.0")}}}),
        json!({"pkg@1.0.0":{}}),
    );
    for (section, key) in [("packages", "pkg@1.0.0"), ("snapshots", "pkg@1.0.0")] {
        let mut broken = baseline.clone();
        broken[section].as_object_mut().unwrap().remove(key);
        assert!(lock(broken).validate().is_err());
    }
    let mut broken = baseline;
    broken["importers"]["app"]["dependencies"]["pkg"]["version"] = json!(null);
    assert!(lock(broken).validate().is_err());
}

#[test]
fn merge_combines_independent_dependencies_and_prunes_both_sides_orphans() {
    let base = lock(graph(json!({"app":{}}), json!({"unused@1.0.0":{}})));
    let ours = lock(graph(
        json!({"app":{"dependencies":{"a":dep("1.0.0")}}}),
        json!({"a@1.0.0":{}}),
    ));
    let theirs = lock(graph(
        json!({"app":{"dependencies":{"b":dep("2.0.0")}}}),
        json!({"b@2.0.0":{}}),
    ));
    let result = merge(&base, &ours, &theirs, None).unwrap();
    assert_eq!(
        result.project().unwrap()["importers"]["app"]["dependencies"],
        json!({"a":dep("1.0.0"),"b":dep("2.0.0")})
    );
    assert_eq!(
        result.project().unwrap()["snapshots"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        Lockfile::parse(&result.to_yaml().unwrap())
            .unwrap()
            .project(),
        result.project()
    );
    assert_eq!(
        merge(&base, &theirs, &ours, None).unwrap().project(),
        result.project()
    );
}

#[test]
fn specifier_and_resolution_are_atomic_even_when_fields_change_independently() {
    let original = graph(
        json!({"app":{"dependencies":{"pkg":{"specifier":"^1.0.0","version":"1.0.0"}}}}),
        json!({"pkg@1.0.0":{},"pkg@1.1.0":{}}),
    );
    let mut ours = original.clone();
    ours["importers"]["app"]["dependencies"]["pkg"]["specifier"] = json!("~1.0.0");
    let mut theirs = original.clone();
    theirs["importers"]["app"]["dependencies"]["pkg"]["version"] = json!("1.1.0");
    let found = conflicts(merge(&lock(original), &lock(ours), &lock(theirs), None).unwrap_err());
    assert_eq!(found[0].path, ["importers", "app", "dependencies", "pkg"]);
}

#[test]
fn delete_modify_conflicts_and_identical_deletions_are_distinguished() {
    let value = graph(
        json!({"app":{"dependencies":{"pkg":dep("1.0.0")}}}),
        json!({"pkg@1.0.0":{},"pkg@2.0.0":{}}),
    );
    let base = lock(value.clone());
    let mut deleted = value.clone();
    deleted["importers"]["app"]["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("pkg");
    let mut changed = value;
    changed["importers"]["app"]["dependencies"]["pkg"] = dep("2.0.0");
    let deleted = lock(deleted);
    let found = conflicts(merge(&base, &deleted, &lock(changed), None).unwrap_err());
    assert_eq!(found[0].kind, ConflictKind::DeleteModify);
    let result = merge(&base, &deleted, &deleted, None).unwrap();
    assert!(
        result.project().unwrap()["snapshots"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn deleting_a_graph_record_does_not_break_a_new_reference_on_the_other_side() {
    let base = lock(graph(
        json!({"old":{"dependencies":{"pkg":dep("1.0.0")}}}),
        json!({"pkg@1.0.0":{}}),
    ));
    let ours = lock(graph(json!({"old":{}}), json!({})));
    let theirs = lock(graph(
        json!({"old":{"dependencies":{"pkg":dep("1.0.0")}},"new":{"dependencies":{"pkg":dep("1.0.0")}}}),
        json!({"pkg@1.0.0":{}}),
    ));
    let result = merge(&base, &ours, &theirs, None).unwrap();
    assert_eq!(result.project().unwrap()["importers"]["old"], json!({}));
    assert!(
        result.project().unwrap()["snapshots"]
            .get("pkg@1.0.0")
            .is_some()
    );
}

#[test]
fn unreachable_graph_conflicts_are_pruned_but_reachable_ones_are_reported() {
    let base_value = graph(json!({"app":{}}), json!({"pkg@1.0.0":{}}));
    let mut ours = base_value.clone();
    ours["snapshots"]["pkg@1.0.0"]["optional"] = json!(true);
    let mut theirs = base_value.clone();
    theirs["snapshots"]["pkg@1.0.0"]["optional"] = json!(false);
    merge(
        &lock(base_value.clone()),
        &lock(ours.clone()),
        &lock(theirs.clone()),
        None,
    )
    .unwrap();
    for value in [&mut ours, &mut theirs] {
        value["importers"]["app"] = json!({"dependencies":{"pkg":dep("1.0.0")}});
    }
    let found = conflicts(merge(&lock(base_value), &lock(ours), &lock(theirs), None).unwrap_err());
    assert_eq!(found[0].path, ["snapshots", "pkg@1.0.0"]);
}

#[test]
fn merge_handles_both_documents() {
    let env = "---\nlockfileVersion: '9.0'\nimporters:\n  .:\n    configDependencies: {}\n";
    let base = Lockfile::parse(&format!(
        "{env}---\nlockfileVersion: '9.0'\nimporters: {{app: {{}}}}\n"
    ))
    .unwrap();
    let ours = Lockfile::parse(&format!(
        "{env}---\nlockfileVersion: '9.0'\nimporters: {{app: {{publishDirectory: dist}}}}\n"
    ))
    .unwrap();
    let theirs = Lockfile::parse(&format!(
        "{}---\nlockfileVersion: '9.0'\nimporters: {{app: {{}}}}\n",
        env.replace(
            "configDependencies: {}",
            "configDependencies: {}\n    custom: yes"
        )
    ))
    .unwrap();
    let result = merge(&base, &ours, &theirs, None).unwrap();
    assert_eq!(result.environment(), theirs.environment());
    assert_eq!(
        result.project().unwrap()["importers"]["app"]["publishDirectory"],
        "dist"
    );
    assert_eq!(
        Lockfile::parse(&result.to_yaml().unwrap())
            .unwrap()
            .document(DocumentKind::Environment),
        theirs.environment()
    );
}

#[test]
fn source_blocks_with_quotes_comments_and_explicit_long_keys_are_preserved() {
    let base = "# baseline\nlockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true # keep\n\nimporters:\n\n  'other': {}\n";
    let key = format!("pkg@1.0.0(peer@{})", "x".repeat(1100));
    let donor = format!(
        "lockfileVersion: '9.0'\nimporters:\n  app:\n    dependencies:\n      pkg:\n        specifier: 1.0.0\n        version: '{}'\npackages:\n  pkg@1.0.0:\n    resolution: {{integrity: sha512-test}}\nsnapshots:\n  ? '{key}'\n  : {{}}\n",
        key.strip_prefix("pkg@").unwrap()
    );
    let result = merge(
        &Lockfile::parse("lockfileVersion: '9.0'\nimporters: {}\n").unwrap(),
        &Lockfile::parse(base).unwrap(),
        &Lockfile::parse(&donor).unwrap(),
        None,
    )
    .unwrap();
    let output = result.to_yaml().unwrap();
    assert!(output.contains("settings:\n  autoInstallPeers: true # keep\n\n"));
    assert!(output.contains("  'other': {}"));
    assert!(output.contains(&format!("  ? '{key}'\n  : {{}}")));
}

#[test]
fn schema_version_spelling_does_not_create_a_conflict() {
    let base = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}}\n").unwrap();
    let ours = Lockfile::parse("lockfileVersion: 9.0\nimporters: {'.': {}}\n").unwrap();
    let theirs = Lockfile::parse("lockfileVersion: 9\nimporters: {'.': {}}\n").unwrap();
    assert_eq!(
        merge(&base, &ours, &theirs, None).unwrap().project(),
        base.project()
    );
}

#[test]
fn catalogs_support_scoped_npm_aliases() {
    let mut value = graph(
        json!({"app":{"dependencies":{"compiler":{"specifier":"catalog:tools","version":"@scope/compiler@6.0.2"}}}}),
        json!({"@scope/compiler@6.0.2":{}}),
    );
    value["catalogs"] =
        json!({"tools":{"compiler":{"specifier":"npm:@scope/compiler@^6.0.0","version":"6.0.2"}}});
    lock(value).validate().unwrap();
}

#[test]
fn inspecting_peer_variants_preserves_legitimate_distinct_instances() {
    let value = graph(
        json!({"app":{"dependencies":{"ui":dep("1.0.0(peer@1.0.0)"),"plugin":dep("1.0.0")}}}),
        json!({
            "ui@1.0.0(peer@1.0.0)":{},"ui@1.0.0(peer@2.0.0)":{},
            "plugin@1.0.0":{"dependencies":{"ui":"1.0.0(peer@2.0.0)"}}
        }),
    );
    let lockfile = lock(value);
    lockfile.validate().unwrap();
    let closure = lockfile.dependency_closure("app").unwrap();
    assert_eq!(closure.variants()["ui@1.0.0"].len(), 2);
    assert_eq!(closure.snapshots.len(), 3);
    assert!(lockfile.dependency_closure("missing").is_err());
}

#[test]
fn a_patch_for_another_package_does_not_authorize_the_hash() {
    let mut value = graph(
        json!({"app":{"dependencies":{"pkg":dep("1.0.0(patch_hash=abc)")}}}),
        json!({"pkg@1.0.0(patch_hash=abc)":{}}),
    );
    value["patchedDependencies"] =
        json!({"another@1.0.0":{"hash":"abc","path":"patches/another.patch"}});
    assert!(lock(value).validate().is_err());
}

#[test]
fn metadata_lists_are_atomic_and_null_is_distinct_from_deletion() {
    let base = lock(
        json!({"lockfileVersion":"9.0","importers":{},"extra":null,"ignoredOptionalDependencies":["a"]}),
    );
    let ours = lock(
        json!({"lockfileVersion":"9.0","importers":{},"ignoredOptionalDependencies":["a","b"]}),
    );
    let theirs = lock(
        json!({"lockfileVersion":"9.0","importers":{},"extra":true,"ignoredOptionalDependencies":["a","c"]}),
    );
    let found = conflicts(merge(&base, &ours, &theirs, None).unwrap_err());
    assert!(
        found
            .iter()
            .any(|c| c.path == ["extra"] && c.kind == ConflictKind::DeleteModify)
    );
    assert!(
        found
            .iter()
            .any(|c| c.path == ["ignoredOptionalDependencies"])
    );
}

#[test]
fn large_lockfiles_are_not_limited_to_a_small_configuration_node_budget() {
    let mut text = String::from("lockfileVersion: '9.0'\nimporters:\n");
    for index in 0..130_000 {
        text.push_str(&format!("  workspace-{index}: {{}}\n"));
    }
    let lockfile = Lockfile::parse(&text).unwrap();
    assert_eq!(
        lockfile.project().unwrap()["importers"]
            .as_object()
            .unwrap()
            .len(),
        130_000
    );
    assert_eq!(lockfile.to_yaml().unwrap(), text);
}

#[test]
fn depth_limits_are_retained_when_the_input_budget_grows() {
    let text = format!(
        "lockfileVersion: '9.0'\nextra: {}null{}\n",
        "[".repeat(1000),
        "]".repeat(1000)
    );
    assert!(Lockfile::parse(&text).is_err());
}

#[test]
fn convergence_uses_manifest_changes_and_preserves_unrelated_candidate_drift() {
    use locksmith::{ManifestChange, converge};
    let baseline = lock(graph(
        json!({"app":{},"other":{}}),
        json!({"old-orphan@1.0.0":{}}),
    ));
    let candidate = lock(graph(
        json!({"app":{"dependencies":{"a":dep("1.0.0")}},"other":{"dependencies":{"b":dep("1.0.0")}}}),
        json!({"a@1.0.0":{},"b@1.0.0":{}}),
    ));
    let before = json!({"name":"app"});
    let after = json!({"name":"app","dependencies":{"a":"1.0.0"}});
    let output = converge(
        &baseline,
        &candidate,
        &[ManifestChange {
            importer: "app",
            before: Some(&before),
            after: Some(&after),
        }],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
    assert_eq!(output.project().unwrap()["importers"]["other"], json!({}));
    assert!(
        output.project().unwrap()["snapshots"]
            .get("a@1.0.0")
            .is_some()
    );
    assert!(
        output.project().unwrap()["snapshots"]
            .get("b@1.0.0")
            .is_none()
    );
    assert!(
        output.project().unwrap()["snapshots"]
            .get("old-orphan@1.0.0")
            .is_some()
    );
}

#[test]
fn script_only_changes_leave_the_lockfile_byte_identical() {
    use locksmith::{ManifestChange, converge};
    let baseline =
        Lockfile::parse("# keep this\nlockfileVersion: '9.0'\nimporters: {app: {}}\n").unwrap();
    let candidate = lock(graph(
        json!({"app":{"dependencies":{"unrelated":dep("1.0.0")}}}),
        json!({"unrelated@1.0.0":{}}),
    ));
    let before = json!({"scripts":{"test":"old"}});
    let after = json!({"scripts":{"test":"new"}});
    let result = converge(
        &baseline,
        &candidate,
        &[ManifestChange {
            importer: "app",
            before: Some(&before),
            after: Some(&after),
        }],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
    assert_eq!(result.to_yaml().unwrap(), baseline.to_yaml().unwrap());
}

#[test]
fn only_the_workspace_root_selects_the_package_manager_for_convergence() {
    let source = lock(graph(json!({".": {}, "app": {}}), json!({})));
    let before = json!({});
    let after = json!({"packageManager": "pnpm@10.13.1"});
    let run = |importer| {
        locksmith::converge(
            &source,
            &source,
            &[locksmith::ManifestChange {
                importer,
                before: Some(&before),
                after: Some(&after),
            }],
            Default::default(),
        )
    };
    assert_eq!(
        run("app").unwrap().to_yaml().unwrap(),
        source.to_yaml().unwrap()
    );
    assert!(matches!(
        run("."),
        Err(LockfileError::GlobalConfigurationChanged { .. })
    ));
}

#[test]
fn global_manifest_configuration_requires_an_explicit_policy() {
    use locksmith::{ManifestChange, converge};
    let baseline = lock(graph(json!({".":{}}), json!({})));
    let before = json!({});
    let after = json!({"pnpm":{"overrides":{"pkg":"2"}}});
    assert!(matches!(
        converge(
            &baseline,
            &baseline,
            &[ManifestChange {
                importer: ".",
                before: Some(&before),
                after: Some(&after)
            }],
            locksmith::ConvergeOptions::default()
        ),
        Err(LockfileError::GlobalConfigurationChanged { .. })
    ));
}

#[test]
fn old_orphans_and_their_references_survive_a_scoped_dependency_removal() {
    let baseline = lock(graph(
        json!({"app":{"dependencies":{"old":dep("1.0.0")}}}),
        json!({"old@1.0.0":{},"orphan@1.0.0":{"dependencies":{"old":"1.0.0"}}}),
    ));
    let donor = lock(graph(json!({"app":{}}), json!({})));
    let output = locksmith::converge(
        &baseline,
        &donor,
        &[locksmith::ManifestChange {
            importer: "app",
            before: Some(&json!({"dependencies":{"old":"1.0.0"}})),
            after: Some(&json!({})),
        }],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["snapshots"],
        baseline.project().unwrap()["snapshots"]
    );
    output.validate().unwrap();
}

#[test]
fn rendering_preserves_nested_unknown_values_and_scalar_types() {
    let metadata = json!({"null":null,"bool":false,"integer":-42,"unsigned":u64::MAX,"fraction":1.25,"text":"9.0","nested":[{"键":"line\nbreak"},[],{},true]});
    let mut base = graph(json!({"app":{}}), json!({}));
    base["extension"] = metadata.clone();
    let donor = lock(graph(json!({"app":{"publishDirectory":"dist"}}), json!({})));
    let result = merge(
        &lock(graph(json!({"app":{}}), json!({}))),
        &lock(base),
        &donor,
        None,
    )
    .unwrap();
    let parsed = Lockfile::parse(&result.to_yaml().unwrap()).unwrap();
    assert_eq!(parsed.project().unwrap()["extension"], metadata);
    assert_eq!(
        parsed.project().unwrap()["importers"]["app"]["publishDirectory"],
        "dist"
    );
}

#[test]
fn convergence_protects_records_used_by_unchanged_dependencies() {
    let base = graph(
        json!({"app":{},"other":{"dependencies":{"shared":dep("1.0.0")}}}),
        json!({"shared@1.0.0":{}}),
    );
    let before = json!({});
    let after = json!({"dependencies":{"shared":"1.0.0"}});
    for section in ["packages", "snapshots"] {
        let mut candidate = base.clone();
        candidate["importers"]["app"]["dependencies"] = json!({"shared":dep("1.0.0")});
        candidate[section]["shared@1.0.0"]["extra"] = json!(true);
        let error = locksmith::converge(
            &lock(base.clone()),
            &lock(candidate),
            &[locksmith::ManifestChange {
                importer: "app",
                before: Some(&before),
                after: Some(&after),
            }],
            locksmith::ConvergeOptions::default(),
        )
        .unwrap_err();
        assert!(conflicts(error).iter().any(
            |c| c.kind == ConflictKind::SharedDependency && c.path == [section, "shared@1.0.0"]
        ));
    }
}
#[test]
fn convergence_protects_release_metadata_across_distinct_peer_instances() {
    let base = graph(
        json!({"app":{}, "other":{"dependencies":{"ui":dep("1.0.0(react@19.0.0)")}}}),
        json!({"ui@1.0.0(react@19.0.0)":{}}),
    );
    let mut candidate = base.clone();
    candidate["importers"]["app"]["dependencies"] = json!({"ui":dep("1.0.0(react@18.0.0)")});
    candidate["snapshots"]["ui@1.0.0(react@18.0.0)"] = json!({});
    candidate["packages"]["ui@1.0.0"]["resolution"]["integrity"] = json!("sha512-changed");
    let error = locksmith::converge(
        &lock(base),
        &lock(candidate),
        &[locksmith::ManifestChange {
            importer: "app",
            before: Some(&json!({})),
            after: Some(&json!({"dependencies":{"ui":"1.0.0"}})),
        }],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap_err();
    assert!(conflicts(error).iter().any(|c| c.kind == ConflictKind::SharedDependency && c.path == ["packages", "ui@1.0.0"]));
}

#[test]
fn aliases_peers_optional_dependencies_cycles_and_files_are_traversed() {
    let mut value = graph(
        json!({"app":{"dependencies":{"alias":dep("real@1.0.0(peer@2.0.0)"),"local":dep("file:../local")}}}),
        json!({
            "real@1.0.0(peer@2.0.0)":{"dependencies":{"peer":"2.0.0"},"optionalDependencies":{"optional":"1.0.0"}},
            "peer@2.0.0":{"dependencies":{"real":"1.0.0(peer@2.0.0)"}},
            "optional@1.0.0":{},"local@file:../local":{}
        }),
    );
    value["packages"]["local@file:../local"]["resolution"] =
        json!({"type":"directory","directory":"../local"});
    let donor = lock(value);
    let closure = donor.dependency_closure("app").unwrap();
    assert_eq!(closure.snapshots.len(), 4);
    donor.validate().unwrap();
}

#[test]
fn contextual_output_renders_unsorted_importers_and_expanded_resolutions_in_pnpm_format() {
    let yaml = "lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\n\nimporters:\n\n  .:\n    dependencies:\n      z:\n        specifier: 1.0.0\n        version: 1.0.0\n      a:\n        specifier: 1.0.0\n        version: 1.0.0\n\npackages:\n\n  a@1.0.0:\n    resolution: {integrity: sha512-a}\n\n  z@1.0.0:\n    resolution:\n      integrity: sha512-z\n\nsnapshots:\n\n  a@1.0.0: {}\n\n  z@1.0.0: {}\n";
    let lock = locksmith::Lockfile::parse(yaml).unwrap();
    assert_eq!(lock.to_yaml().unwrap(), yaml);
    let manifests = std::collections::BTreeMap::from([(
        ".".into(),
        serde_json::json!({"dependencies":{"z":"1.0.0","a":"1.0.0"}}),
    )]);
    let cfg = locksmith::WorkspaceConfig::default();
    let out = locksmith::merge(
        &lock,
        &lock,
        &lock,
        Some(&locksmith::MergeContext {
            configuration: &cfg,
            configurations: [&cfg; 3],
            target: locksmith::MergeTarget::Ours,
            registry: None,
            manifests: &manifests,
            patches: &std::collections::BTreeMap::new(),
        }),
    )
    .unwrap()
    .to_yaml()
    .unwrap();
    assert!(out.contains("    resolution: {integrity: sha512-z}\n"));
    assert!(out.find("      a:\n").unwrap() < out.find("      z:\n").unwrap());
}
