use super::{cfg, lock};
use locksmith::{
    ConvergeOptions, Lockfile, LockfileError, MergeContext, MergeTarget, PeerRules, RepairContext,
    Value, WorkspaceConfig, converge, inspect_peers, merge, repair,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn empty_workspace_rules_stay_absent_in_the_merged_lockfile() {
    let input = Lockfile::parse("lockfileVersion: '9.0'\nsettings:\n  autoInstallPeers: true\n  excludeLinksFromLockfile: false\nimporters:\n  .: {}\n").unwrap();
    let config = cfg(&json!({"overrides": {}, "patchedDependencies": {}}));
    let result = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &BTreeMap::from([(".".into(), json!({}))]),
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    let project = result.project().unwrap();
    assert!(project.get("overrides").is_none());
    assert!(project.get("patchedDependencies").is_none());
}

#[test]
fn unmaterialized_releases_use_their_locked_peer_instances() {
    let mut data = lock(1, 1, false).project().unwrap().clone();
    data["importers"]["."]["dependencies"]["lib"]["version"] = json!("1.0.0");
    let input = Lockfile::parse(&data.to_string()).unwrap();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"1.0.0","b":"1.0.0","lib":"1.0.0"}}),
    )]);
    let config = WorkspaceConfig::default();
    let output = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    output.validate().unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["."]["dependencies"]["lib"]["version"],
        "1.0.0(a@1.0.0)(b@1.0.0)"
    );

    data["packages"]["wrapper@1.0.0"] = json!({"resolution":{"integrity":"sha512-wrapper"}});
    data["snapshots"]["wrapper@1.0.0"] = json!({"dependencies":{"lib":"1.0.0"}});
    data["importers"]["."]["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("lib");
    data["importers"]["."]["dependencies"]["wrapper"] =
        json!({"specifier":"1.0.0","version":"1.0.0"});
    let nested = Lockfile::parse(&data.to_string()).unwrap();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"1.0.0","b":"1.0.0","wrapper":"1.0.0"}}),
    )]);
    let output = merge(
        &nested,
        &nested,
        &nested,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    output.validate().unwrap();
    assert_eq!(
        output.project().unwrap()["snapshots"]["wrapper@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["lib"],
        "1.0.0(a@1.0.0)(b@1.0.0)"
    );
}

#[test]
fn final_manifest_and_target_preference_resolve_competing_pins_and_peer_cycles() {
    for cycle in [false, true] {
        let base = lock(1, 1, cycle);
        let ours = lock(2, 2, cycle);
        let theirs = lock(3, 1, cycle);
        let manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"a":"*","b":"2.0.0","lib":"1.0.0"}}),
        )]);
        let config = json!({});
        let patches = BTreeMap::new();
        let normalized_config = cfg(&config);
        let context = MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&normalized_config; 3],
            configuration: &normalized_config,
            patches: &patches,
            registry: None,
        };
        let result = merge(&base, &ours, &theirs, Some(&context)).unwrap();
        result.validate().unwrap();
        let swapped = MergeContext {
            target: MergeTarget::Ours,
            registry: None,
            ..context
        };
        assert_eq!(
            merge(&base, &theirs, &ours, Some(&swapped))
                .unwrap()
                .project(),
            result.project()
        );
        assert_eq!(
            merge(&result, &result, &result, Some(&swapped))
                .unwrap()
                .project(),
            result.project()
        );
        let doc = result.project().unwrap();
        assert_eq!(
            doc["importers"]["."]["dependencies"]["a"]["version"],
            "3.0.0"
        );
        assert_eq!(
            doc["importers"]["."]["dependencies"]["b"]["version"],
            "2.0.0"
        );
        assert_eq!(
            doc["importers"]["."]["dependencies"]["lib"]["version"],
            "1.0.0(a@3.0.0)(b@2.0.0)"
        );
        if cycle {
            assert_eq!(
                doc["snapshots"]["wrapper@1.0.0(a@3.0.0)(b@2.0.0)"]["dependencies"]["lib"],
                "1.0.0(a@3.0.0)(b@2.0.0)"
            );
        }
        assert_eq!(
            Lockfile::parse(&result.to_yaml().unwrap())
                .unwrap()
                .project(),
            result.project()
        );
    }
}

#[test]
fn unavailable_versions_are_reported_with_the_affected_dependency() {
    let base = lock(1, 1, false);
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"a":"4.0.0"}}))]);
    let config = json!({});
    let patches = BTreeMap::new();
    let normalized_config = cfg(&config);
    let context = MergeContext {
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
        registry: None,
    };
    assert!(
        matches!(merge(&base,&base,&base,Some(&context)),Err(LockfileError::ResolutionRequired {path,..}) if path==["importers",".","dependencies","a"])
    );
}

#[test]
fn final_catalog_decides_the_pin_and_new_consumers_use_that_pin() {
    let make = |version: u8, consumer: bool| {
        let mut data = lock(version, 1, false).project().unwrap().clone();
        data["catalogs"] = json!({"default":{"a":{"specifier":format!("{version}.0.0"),"version":format!("{version}.0.0")}}});
        data["importers"]["."]["dependencies"]["a"]["specifier"] = json!("catalog:");
        if consumer {
            data["importers"]["consumer"] = json!({"dependencies":{"a":{"specifier":"catalog:","version":format!("{version}.0.0")}}});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, false);
    let ours = make(2, false);
    let theirs = make(1, true);
    let manifests = BTreeMap::from([
        (
            ".".into(),
            json!({"dependencies":{"a":"catalog:","b":"*","lib":"1.0.0"}}),
        ),
        ("consumer".into(), json!({"dependencies":{"a":"catalog:"}})),
    ]);
    let old = json!({"catalog":{"a":"1.0.0"}});
    let new = json!({"catalog":{"a":"2.0.0"}});
    let patches = BTreeMap::new();
    let normalized_old = cfg(&old);
    let normalized_new = cfg(&new);
    let context = MergeContext {
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_old, &normalized_new, &normalized_old],
        configuration: &normalized_new,
        patches: &patches,
        registry: None,
    };
    let result = merge(&base, &ours, &theirs, Some(&context)).unwrap();
    result.validate().unwrap();
    for id in [".", "consumer"] {
        assert_eq!(
            result.project().unwrap()["importers"][id]["dependencies"]["a"]["version"],
            "2.0.0"
        );
    }
}

#[test]
fn same_snapshot_regular_dependency_disagreement_uses_the_target_graph() {
    let make = |runtime: u8| {
        let mut data: Value = lock(1, 1, false).project().unwrap().clone();
        data["packages"][format!("runtime@{runtime}.0.0")] =
            json!({"resolution":{"integrity":"sha512-runtime"}});
        data["snapshots"][format!("runtime@{runtime}.0.0")] = json!({});
        data["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["runtime"] =
            json!(format!("{runtime}.0.0"));
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1);
    let ours = make(2);
    let theirs = make(3);
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"*","b":"*","lib":"1.0.0"}}),
    )]);
    let config = json!({});
    let patches = BTreeMap::new();
    let normalized_config = cfg(&config);
    let context = MergeContext {
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
        registry: None,
    };
    let result = merge(&base, &ours, &theirs, Some(&context)).unwrap();
    assert_eq!(
        result.project().unwrap()["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["runtime"],
        "3.0.0"
    );
}

#[test]
fn new_catalog_alias_uses_an_already_locked_package() {
    let input = lock(1, 1, false);
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"alias":"catalog:"}}))]);
    let before = json!({});
    let after = json!({"catalog":{"alias":"npm:a@1.0.0"}});
    let patches = BTreeMap::new();
    let normalized_before = cfg(&before);
    let normalized_after = cfg(&after);
    let context = MergeContext {
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_before; 3],
        configuration: &normalized_after,
        patches: &patches,
        registry: None,
    };
    let output = merge(&input, &input, &input, Some(&context)).unwrap();
    output.validate().unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["."]["dependencies"]["alias"]["version"],
        "a@1.0.0"
    );
    assert_eq!(
        output.project().unwrap()["catalogs"]["default"]["alias"]["version"],
        "1.0.0"
    );
}

#[test]
fn workspace_protocol_with_whitespace_retains_the_existing_link() {
    let mut data = lock(1, 1, false).project().unwrap().clone();
    data["importers"]["."]["dependencies"]["local"] =
        json!({"specifier":"workspace: *","version":"link:packages/local"});
    data["importers"]["packages/local"] = json!({});
    let input = Lockfile::parse(&data.to_string()).unwrap();
    let manifests = BTreeMap::from([
        (".".into(), json!({"dependencies":{"local":"workspace: *"}})),
        ("packages/local".into(), json!({"name":"local"})),
    ]);
    let config = json!({});
    let patches = BTreeMap::new();
    let normalized_config = cfg(&config);
    let context = MergeContext {
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
        registry: None,
    };
    let output = merge(&input, &input, &input, Some(&context)).unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["."]["dependencies"]["local"]["version"],
        "link:packages/local"
    );
}

#[test]
fn linked_peers_keep_root_relative_edges_when_workspace_paths_change() {
    use sha2::{Digest, Sha256};
    for hashed in [false, true] {
        let suffix = |path: &str, b: u8| {
            let text = format!("b@{b}.0.0)(provider@{}", path.replace('/', "+"));
            if hashed {
                let hash: String = Sha256::digest(text.as_bytes())[..16]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                format!("({hash})")
            } else {
                format!("({text})")
            }
        };
        let make = |path: &str, b: u8| {
            let peers = suffix(path, b);
            Lockfile::parse(&json!({
                "lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
                "importers":{
                    ".":{},path:{},
                    "packages/app":{"dependencies":{
                        "provider":{"specifier":"workspace:*","version":format!("link:../{}", path.strip_prefix("packages/").unwrap())},
                        "b":{"specifier":format!("{b}.0.0"),"version":format!("{b}.0.0")},
                        "wrapper":{"specifier":"1.0.0","version":format!("1.0.0{peers}")}
                    }}
                },
                "packages":{
                    format!("b@{b}.0.0"):{"resolution":{"integrity":"sha512-b"}},
                    "wrapper@1.0.0":{"resolution":{"integrity":"sha512-wrapper"}},
                    "lib@1.0.0":{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"provider":"^1","b":"*"}}
                },
                "snapshots":{
                    format!("b@{b}.0.0"): {},
                    format!("wrapper@1.0.0{peers}"):{"dependencies":{"lib":format!("1.0.0{peers}")},"transitivePeerDependencies":["b","provider"]},
                    format!("lib@1.0.0{peers}"):{"dependencies":{"provider":format!("link:{path}"),"b":format!("{b}.0.0")}}
                }
            }).to_string()).unwrap()
        };
        let old = "packages/old";
        let new = "packages/@scope/new";
        let manifests = BTreeMap::from([
            (".".into(), json!({"private":true})),
            (new.into(), json!({"name":"provider","version":"1.0.0"})),
            (
                "packages/app".into(),
                json!({"dependencies":{"provider":"workspace:*","b":"2.0.0","wrapper":"1.0.0"}}),
            ),
        ]);
        let config = cfg(&json!({}));
        let output = merge(
            &make(old, 1),
            &make(old, 2),
            &make(new, 1),
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        )
        .unwrap();
        output.validate().unwrap();
        assert_eq!(output.project(), make(new, 2).project());
        assert!(inspect_peers(&output, &PeerRules::default(), Some(&manifests)).is_empty());
        let issues = inspect_peers(&output, &PeerRules::default(), None);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].peer, "provider");
        let mut incompatible = manifests.clone();
        incompatible.get_mut(new).unwrap()["version"] = json!("2.0.0");
        let issues = inspect_peers(&output, &PeerRules::default(), Some(&incompatible));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].wanted, "^1");
        let rules = PeerRules {
            allowed_versions: vec![("provider".into(), "^2".into())],
            ..PeerRules::default()
        };
        assert!(inspect_peers(&output, &rules, Some(&incompatible)).is_empty());
        let symmetric =
            locksmith::merge(&make(old, 1), &make(old, 2), &make(new, 1), None).unwrap();
        assert_eq!(symmetric.project(), make(new, 2).project());
    }
}

#[test]
fn changed_resolution_configuration_uses_an_input_with_that_configuration() {
    let input = lock(1, 1, false);
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"a":"1.0.0"}}))]);
    let before = json!({});
    let after = json!({"packageExtensions":{"a":{"dependencies":{"extra":"*"}}}});
    let patches = BTreeMap::new();
    assert!(matches!(
        merge(
            &input,
            &input,
            &input,
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&cfg(&before); 3],
                configuration: &cfg(&after),
                patches: &patches,
                registry: None,
            })
        ),
        Err(LockfileError::ResolutionRequired { .. })
    ));
}

#[test]
fn new_workspace_links_are_derived_from_final_manifests() {
    let input = lock(1, 1, false);
    let manifests = BTreeMap::from([
        (".".into(), json!({})),
        (
            "apps/app".into(),
            json!({"name":"app","dependencies":{"alias":"workspace:@local/lib@^1"}}),
        ),
        (
            "packages/lib".into(),
            json!({"name":"@local/lib","version":"1.0.0","dependencies":{"a":"1.0.0"}}),
        ),
    ]);
    let config = json!({});
    let patches = BTreeMap::new();
    let output = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&config); 3],
            configuration: &cfg(&config),
            patches: &patches,
            registry: None,
        }),
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["apps/app"]["dependencies"]["alias"]["version"],
        "link:../../packages/lib"
    );
    output.validate().unwrap();
}

#[test]
fn existing_workspace_links_must_satisfy_the_final_workspace_version() {
    let value = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"lib":{"specifier":"workspace:^1","version":"link:lib"}}},"lib":{}}});
    let input = Lockfile::parse(&value.to_string()).unwrap();
    let manifests = BTreeMap::from([
        (".".into(), json!({"dependencies":{"lib":"workspace:^1"}})),
        ("lib".into(), json!({"name":"lib","version":"2.0.0"})),
    ]);
    let config = json!({});
    let patches = BTreeMap::new();
    assert!(
        matches!(merge(&input,&input,&input,Some(&MergeContext{registry:None,target:MergeTarget::Theirs,manifests:&manifests,configurations:[&cfg(&config);3],configuration:&cfg(&config),patches:&patches})),Err(LockfileError::ResolutionRequired{path,..}) if path==["importers",".","dependencies","lib"])
    );
}

#[test]
fn mainline_pins_survive_unrelated_configuration_edits_until_the_branch_requires_a_change() {
    let mainline = lock(1, 1, false);
    let topic = lock(2, 1, false);
    let before = WorkspaceConfig::default();
    let after = cfg(&json!({"packageExtensions":{"unrelated":{"dependencies":{"extra":"1.0.0"}}}}));
    for (required, expected) in [("*", "1.0.0"), ("^2", "2.0.0")] {
        let manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"a":required,"b":"*","lib":"1.0.0"}}),
        )]);
        let result = merge(
            &mainline,
            &topic,
            &mainline,
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&before, &after, &before],
                configuration: &after,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        )
        .unwrap();
        let pins = &result.project().unwrap()["importers"]["."]["dependencies"];
        assert_eq!(pins["a"]["version"], expected);
        assert_eq!(
            pins["lib"]["version"],
            format!("1.0.0(a@{expected})(b@1.0.0)")
        );
    }
}

#[test]
fn matching_configuration_candidates_precede_other_configurations_direct_pins() {
    let base = lock(1, 1, false);
    let mut ours = lock(3, 1, false).project().unwrap().clone();
    let opaque = "lib@1.0.0(ffffffffffffffffffffffffffffffff)";
    let body = ours["snapshots"]
        .as_object_mut()
        .unwrap()
        .remove("lib@1.0.0(a@3.0.0)(b@1.0.0)")
        .unwrap();
    ours["snapshots"][opaque] = body;
    ours["importers"]["."]["dependencies"]["lib"]["version"] =
        json!("1.0.0(ffffffffffffffffffffffffffffffff)");
    let ours = Lockfile::parse(&ours.to_string()).unwrap();
    let mut theirs = lock(2, 1, false).project().unwrap().clone();
    let dependency = theirs["importers"]["."]["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("lib")
        .unwrap();
    theirs["importers"]["other"] = json!({"dependencies":{"lib":dependency}});
    theirs["packageExtensionsChecksum"] = json!("sha256-final");
    let theirs = Lockfile::parse(&theirs.to_string()).unwrap();
    let before = WorkspaceConfig::default();
    let after = cfg(&json!({"packageExtensions":{"unrelated":{"dependencies":{"extra":"1.0.0"}}}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"2.0.0","b":"1.0.0","lib":"1.0.0"}}),
    )]);
    let patches = BTreeMap::new();
    let result = merge(
        &base,
        &ours,
        &theirs,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&before, &before, &after],
            configuration: &after,
            patches: &patches,
            registry: None,
        }),
    )
    .unwrap();
    result.validate().unwrap();
    assert_eq!(
        result.project().unwrap()["importers"]["."]["dependencies"]["lib"]["version"],
        "1.0.0(a@2.0.0)(b@1.0.0)"
    );
}

#[test]
fn duplicate_workspace_names_only_block_a_link_that_uses_the_name() {
    let input =
        Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}, a: {}, b: {}}").unwrap();
    let manifests = BTreeMap::from([
        (".".into(), json!({})),
        ("a".into(), json!({"name":"shared","version":"1.0.0"})),
        ("b".into(), json!({"name":"shared","version":"1.0.0"})),
    ]);
    let config = cfg(&json!({}));
    let patches = BTreeMap::new();
    let merge = |manifests| {
        let context = MergeContext {
            target: MergeTarget::Ours,
            manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &patches,
            registry: None,
        };
        merge(&input, &input, &input, Some(&context))
    };
    merge(&manifests).unwrap();
    let mut linked = manifests.clone();
    linked.get_mut(".").unwrap()["dependencies"] = json!({"shared":"workspace:*"});
    let error = merge(&linked).unwrap_err();
    assert!(
        matches!(error, LockfileError::ResolutionRequired { path, .. }
        if path == ["importers", ".", "shared"])
    );
}

#[test]
fn workspace_sources_and_peer_overrides_follow_the_effective_manifest() {
    let input = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}}").unwrap();
    let manifests = BTreeMap::from([
        (".".into(), json!({})),
        (
            "tools/tsconfig".into(),
            json!({"name":"tsconfig","version":"0.0.0"}),
        ),
        ("lib".into(), json!({"name":"lib","version":"1.2.0"})),
        (
            "app".into(),
            json!({
                "dependencies":{"tsconfig":"workspace:","local":"npm:lib@^1"},
                "devDependencies":{"lib":"workspace:^"},
                "peerDependencies":{"lib":"workspace:^","lint":"catalog:"}
            }),
        ),
    ]);
    let config = cfg(
        &json!({"autoInstallPeers":false,"linkWorkspacePackages":true,"overrides":{"lib":"link:lib"},"patchedDependencies":null}),
    );
    let result = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    assert_eq!(
        result.project().unwrap()["importers"]["app"],
        json!({"dependencies":{
            "tsconfig":{"specifier":"workspace:","version":"link:../tools/tsconfig"},
            "local":{"specifier":"npm:lib@^1","version":"link:../lib"},
            "lib":{"specifier":"link:../lib","version":"link:../lib"}
        }})
    );
}

#[test]
fn final_manager_selects_its_bootstrap_graph_and_runtime_pin() {
    let make = |manager: &str| {
        let env = json!({"lockfileVersion":"9.0","importers":{".":{"configDependencies":{},"packageManagerDependencies":{"pnpm":{"specifier":manager,"version":manager}}}},
            "packages":{format!("pnpm@{manager}"):{"resolution":{"integrity":"sha512-manager"}}},"snapshots":{format!("pnpm@{manager}"): {}}});
        let project = json!({"lockfileVersion":"9.0","importers":{".":{"devDependencies":{"node":{"specifier":"runtime:24.0.0","version":"runtime:24.0.0"}}}},
          "packages":{"node@runtime:24.0.0":{"resolution":{"type":"variations"}}},"snapshots":{"node@runtime:24.0.0":{}}});
        Lockfile::parse(&format!("---\n{env}\n---\n{project}\n")).unwrap()
    };
    let old = make("12.4.0");
    let new = make("12.6.0");
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"packageManager":"pnpm@12.6.0+sha512.abcdef","devEngines":{"runtime":[{"name":"node","version":"24.0.0","onFail":"download"}]}}),
    )]);
    let config = WorkspaceConfig::default();
    let result = merge(
        &old,
        &old,
        &new,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    assert_eq!(result.environment(), new.environment());
    assert_eq!(
        result.project().unwrap()["importers"]["."]["devDependencies"]["node"]["version"],
        "runtime:24.0.0"
    );
}

#[test]
fn direct_aliases_keep_bare_version_pins_and_prune_stale_publication_times() {
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"a":"npm:a@1.0.0"}}))]);
    let config = WorkspaceConfig::default();
    for prefix in ["", "/"] {
        let mut data = lock(1, 1, false).project().unwrap().clone();
        let kept = format!("{prefix}a@1.0.0");
        let stale = format!("{prefix}b@1.0.0");
        data["time"] = json!({&kept:"2026-01-01T00:00:00.000Z",stale:"2026-02-01T00:00:00.000Z"});
        let input = Lockfile::parse(&data.to_string()).unwrap();
        let result = merge(
            &input,
            &input,
            &input,
            Some(&MergeContext {
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        )
        .unwrap();
        let data = result.project().unwrap();
        assert_eq!(
            data["importers"]["."]["dependencies"]["a"]["version"],
            "1.0.0"
        );
        assert_eq!(data["time"], json!({kept:"2026-01-01T00:00:00.000Z"}));
    }
}

#[test]
fn numeric_npm_aliases_follow_the_runner_for_direct_and_catalog_pins() {
    for (runner, target, pin) in [("10.34.9", "1", "1@1.2.0"), ("11.1.0", "foo", "1.2.0")] {
        let runner = runner.parse().unwrap();
        for catalog in [false, true] {
            let make = |specifier: &str, actual: &str, pin: &str| {
                let authored = if catalog { "catalog:" } else { specifier };
                let key = format!("{actual}@1.2.0");
                let mut project = json!({"lockfileVersion":"9.0",
                    "importers":{".":{"dependencies":{"foo":{"specifier":authored,"version":pin}}}},
                    "packages":{&key:{"resolution":{}}},"snapshots":{&key:{}}});
                let mut raw = json!({});
                if catalog {
                    project["catalogs"] =
                        json!({"default":{"foo":{"specifier":specifier,"version":"1.2.0"}}});
                    raw["catalog"] = json!({"foo":specifier});
                }
                (
                    Lockfile::parse(&project.to_string()).unwrap(),
                    WorkspaceConfig::from_pnpm_version(&raw, &runner).unwrap(),
                )
            };
            let (base, before) = make("^1", "foo", "1.2.0");
            let (other, after) = make("npm:1", target, pin);
            let authored = if catalog { "catalog:" } else { "npm:1" };
            let manifests =
                BTreeMap::from([(".".into(), json!({"dependencies":{"foo":authored}}))]);
            let merged = merge(
                &base,
                &base,
                &other,
                Some(&MergeContext {
                    target: MergeTarget::Ours,
                    manifests: &manifests,
                    configurations: [&before, &before, &after],
                    configuration: &after,
                    patches: &BTreeMap::new(),
                    registry: None,
                }),
            )
            .unwrap();
            assert_eq!(
                merged.project().unwrap()["importers"]["."]["dependencies"]["foo"]["version"],
                pin
            );
        }
    }
}

#[test]
fn registry_pin_and_matching_workspace_release_follow_final_link_policy() {
    let lock=Lockfile::parse(&json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"foo":{"specifier":"^1","version":"1.1.0"}}},"packages/foo":{}},"packages":{"foo@1.1.0":{"resolution":{"integrity":"sha512-test"}}},"snapshots":{"foo@1.1.0":{}}}).to_string()).unwrap();
    for (local, prefer, expected) in [
        ("1.1.0", false, "link:packages/foo"),
        ("1.0.0", false, "1.1.0"),
        ("1.0.0", true, "link:packages/foo"),
    ] {
        let manifests = BTreeMap::from([
            (".".into(), json!({"dependencies":{"foo":"^1"}})),
            ("packages/foo".into(), json!({"name":"foo","version":local})),
        ]);
        let config = cfg(&json!({"linkWorkspacePackages":true,"preferWorkspacePackages":prefer}));
        let baseline = WorkspaceConfig::default();
        let merged = merge(
            &lock,
            &lock,
            &lock,
            Some(&MergeContext {
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&baseline, &baseline, &config],
                configuration: &config,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        )
        .unwrap();
        assert_eq!(
            merged.project().unwrap()["importers"]["."]["dependencies"]["foo"]["version"],
            expected
        );
    }
}

#[test]
fn merge_prefers_a_legacy_catalog_self_link_over_a_registry_release() {
    let config = cfg(&json!({"catalog":{"app":"^1"},"linkWorkspacePackages":true}));
    let manifests = BTreeMap::from([
        (".".into(), json!({})),
        (
            "app".into(),
            json!({"name":"app","version":"1.2.0","dependencies":{"app":"catalog:"}}),
        ),
    ]);
    let local = Lockfile::parse(&json!({
        "lockfileVersion":"9.0",
        "catalogs":{"default":{"app":{"specifier":"^1","version":"^1"}}},
        "importers":{".":{},"app":{"dependencies":{"app":{"specifier":"catalog:","version":"link:"}}}}
    }).to_string()).unwrap();
    let registry = Lockfile::parse(&json!({
        "lockfileVersion":"9.0",
        "catalogs":{"default":{"app":{"specifier":"^1","version":"1.9.0"}}},
        "importers":{".":{},"app":{"dependencies":{"app":{"specifier":"catalog:","version":"1.9.0"}}}},
        "packages":{"app@1.9.0":{"resolution":{}}},
        "snapshots":{"app@1.9.0":{}}
    }).to_string()).unwrap();
    let merged = merge(
        &local,
        &local,
        &registry,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    assert_eq!(
        merged.project().unwrap()["importers"]["app"]["dependencies"]["app"]["version"],
        "link:"
    );
}

#[test]
fn catalog_links_use_the_local_manifest_version_with_context() {
    let config = cfg(&json!({"catalogs":{"local":{"lib":"^1"}},"linkWorkspacePackages":true}));
    let mut value = json!({"lockfileVersion":"9.0","catalogs":{"local":{"lib":{"specifier":"^1","version":"1.2.0"}}},
        "importers":{".":{"dependencies":{"lib":{"specifier":"catalog:local","version":"link:packages/lib"}}},"packages/lib":{}}});
    let input = Lockfile::parse(&value.to_string()).unwrap();
    let plain = merge(&input, &input, &input, None).unwrap();
    assert_eq!(plain.project().unwrap(), input.project().unwrap());
    for (version, compatible) in [("1.2.0", true), ("2.0.0", false)] {
        let manifests = BTreeMap::from([
            (".".into(), json!({"dependencies":{"lib":"catalog:local"}})),
            (
                "packages/lib".into(),
                json!({"name":"lib","version":version}),
            ),
        ]);
        let result = merge(
            &input,
            &input,
            &input,
            Some(&MergeContext {
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        );
        if compatible {
            assert_eq!(
                result.unwrap().project().unwrap()["importers"]["."]["dependencies"]["lib"]["version"],
                "link:packages/lib"
            );
        } else {
            assert!(
                result.is_err(),
                "an incompatible workspace version cannot satisfy the catalog"
            );
        }
    }
    value["importers"]["."]["dependencies"]["lib"]["version"] = json!("link:missing");
    let invalid = Lockfile::parse(&value.to_string()).unwrap();
    assert!(merge(&invalid, &invalid, &invalid, None).is_err());
}

#[test]
fn catalog_links_can_target_a_workspace_packages_external_publish_directory() {
    let mut value = json!({
        "lockfileVersion":"9.0",
        "catalogs":{"default":{"lib":{"specifier":"^1","version":"1.2.0"}}},
        "importers":{
            ".":{"dependencies":{"lib":{"specifier":"catalog:","version":"link:dist/lib"}}},
            "packages/lib":{"publishDirectory":"../../dist/lib"}
        }
    });
    let input = Lockfile::parse(&value.to_string()).unwrap();
    let output = merge(&input, &input, &input, None).unwrap();
    assert_eq!(output.to_yaml().unwrap(), input.to_yaml().unwrap());

    value["importers"]["packages/lib"]["linkDirectory"] = json!(false);
    let unlinked = Lockfile::parse(&value.to_string()).unwrap();
    assert!(merge(&unlinked, &unlinked, &unlinked, None).is_err());
}

#[test]
fn bootstrap_follows_the_final_config_dependency_declarations() {
    let project = json!({"lockfileVersion":"9.0","importers":{".":{}}});
    let make = |version: &str| {
        let env = json!({"lockfileVersion":"9.0","importers":{".":{"configDependencies":{"config":{"specifier":version,"version":version}}}},
            "packages":{format!("config@{version}"):{"resolution":{"integrity":"sha512-example"}}},"snapshots":{format!("config@{version}"): {}}});
        Lockfile::parse(&format!("---\n{env}\n---\n{project}\n")).unwrap()
    };
    let old = make("1.0.0");
    let new = make("2.0.0");
    let manifests = BTreeMap::from([(".".into(), json!({}))]);
    let config =
        |v: &str| WorkspaceConfig::from_pnpm(&json!({"configDependencies":{"config":v}})).unwrap();
    let before = config("1.0.0");
    let after = config("2.0.0");
    let ctx = MergeContext {
        target: MergeTarget::Ours,
        manifests: &manifests,
        configurations: [&before, &before, &after],
        configuration: &after,
        patches: &BTreeMap::new(),
        registry: None,
    };
    let merged = merge(&old, &old, &new, Some(&ctx)).unwrap();
    assert_eq!(merged.environment(), new.environment());
    let converged = converge(
        &old,
        &new,
        &[],
        ConvergeOptions {
            configuration: Some(&after),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(converged.environment(), new.environment());
    converged
        .validate_workspace(&manifests, &Default::default(), &after, ctx.patches, |_| {
            Ok(None)
        })
        .unwrap();
    assert!(
        old.validate_workspace(
            &manifests,
            &Default::default(),
            &after,
            ctx.patches,
            |_| Ok(None)
        )
        .is_err()
    );
    assert!(merge(&old, &old, &old, Some(&ctx)).is_err());
    let mut incorrect = new.environment().unwrap().clone();
    incorrect["importers"]["."]["configDependencies"]["config"]["specifier"] = json!("1.0.0");
    let incorrect = Lockfile::parse(&format!("---\n{incorrect}\n---\n{project}\n")).unwrap();
    assert!(
        incorrect
            .validate_workspace(
                &manifests,
                &Default::default(),
                &before,
                ctx.patches,
                |_| Ok(None)
            )
            .is_err()
    );
    let tagged = config("latest");
    for (pin, valid) in [("2.0.0", true), ("link:./local", false)] {
        let mut env = new.environment().unwrap().clone();
        env["importers"]["."]["configDependencies"]["config"] =
            json!({"specifier":"latest","version":pin});
        let lock = Lockfile::parse(&format!("---\n{env}\n---\n{project}\n")).unwrap();
        assert_eq!(
            lock.validate_workspace(
                &manifests,
                &Default::default(),
                &tagged,
                ctx.patches,
                |_| Ok(None)
            )
            .is_ok(),
            valid,
            "{pin}"
        );
    }
    for (pin, valid) in [("2.0.0", true), ("link:./local", false)] {
        assert_eq!(
            make(pin)
                .validate_workspace(
                    &manifests,
                    &Default::default(),
                    &config(&format!("{pin}+sha512-example")),
                    ctx.patches,
                    |_| Ok(None),
                )
                .is_ok(),
            valid,
            "{pin}"
        );
    }
    for (url, valid) in [
        ("https://registry.test/config/-/config-2.0.0.tgz", true),
        ("https://registry.test/artifacts/custom.tgz", false),
        ("https://mirror.test/config/-/config-2.0.0.tgz", false),
        (
            "https://registry.test/custom?file=/config/-/config-2.0.0.tgz",
            false,
        ),
    ] {
        let cfg = WorkspaceConfig::from_pnpm(
            &json!({"registry":"https://registry.test","configDependencies":{"config":{
                "integrity":"2.0.0+sha512-example","tarball":url
            }}}),
        )
        .unwrap();
        assert_eq!(
            new.validate_workspace(&manifests, &Default::default(), &cfg, ctx.patches, |_| Ok(
                None
            ))
            .is_ok(),
            valid
        );
    }
    let malformed = WorkspaceConfig::from_pnpm(&json!({"configDependencies":{"config":{
        "integrity":"2.0.0","tarball":"https://mirror.test/config-2.0.0.tgz"
    }}}))
    .unwrap();
    assert!(
        new.validate_workspace(
            &manifests,
            &Default::default(),
            &malformed,
            ctx.patches,
            |_| Ok(None)
        )
        .is_err()
    );
    let mut broken = new.environment().unwrap().clone();
    broken["snapshots"] = json!({});
    let broken = Lockfile::parse(&format!("---\n{broken}\n---\n{project}\n")).unwrap();
    assert_eq!(
        merge(&old, &broken, &new, Some(&ctx))
            .unwrap()
            .environment(),
        new.environment()
    );
}

#[test]
fn merge_and_repair_validate_bootstrap_settings_for_the_runner() {
    for (runner, quoted_supported) in [("11.28.12", false), ("12.21.0", true)] {
        let make = |setting| {
            let key = format!("pnpm@{runner}");
            let environment = json!({"lockfileVersion":"9.0","settings":{"autoInstallPeers":setting},
                "importers":{".":{"packageManagerDependencies":{"pnpm":{"specifier":runner,"version":runner}}}},
                "packages":{&key:{"resolution":{}}},"snapshots":{&key:{}}});
            format!("---\n{environment}\n---\nlockfileVersion: '9.0'\nimporters: {{.: {{}}}}\n")
        };
        let plain = make(json!(true));
        let quoted = make(json!("true"));
        let config =
            WorkspaceConfig::from_pnpm_version(&json!({}), &runner.parse().unwrap()).unwrap();
        let manifests = BTreeMap::from([(".".into(), json!({}))]);
        let patches = BTreeMap::new();
        let merged = merge(
            plain.as_str(),
            quoted.as_str(),
            plain.as_str(),
            Some(&MergeContext {
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &patches,
                registry: None,
            }),
        )
        .unwrap();
        let expected = if quoted_supported {
            json!("true")
        } else {
            json!(true)
        };
        assert_eq!(
            merged.environment().unwrap()["settings"]["autoInstallPeers"],
            expected
        );

        let context = RepairContext {
            registry: None,
            manifests: &manifests,
            configuration: &config,
            patches: &patches,
            parents: &[],
        };
        assert_eq!(repair(&plain, &context).unwrap().to_yaml().unwrap(), plain);
        if quoted_supported {
            assert_eq!(
                repair(&quoted, &context).unwrap().to_yaml().unwrap(),
                quoted
            );
        } else {
            assert!(
                matches!(repair(&quoted, &context), Err(LockfileError::InvalidGraph(issues))
                if issues.iter().any(|issue| issue.path == ["settings"]))
            );
        }
    }
}

#[test]
fn branch_configurations_use_the_source_pnpm_version_before_the_final_manifest() {
    let project = "lockfileVersion: '9.0'\nimporters: {.: {}}\n";
    let old = r#"{"lockfileVersion":"9.0","importers":{".":{"packageManagerDependencies":{"pnpm":{"specifier":"12.18.0","version":"12.18.0"}}}}}"#;
    let yaml = format!("---\n{old}\n---\n{project}");
    let source = locksmith::MergeInput::from(yaml.as_str());
    let no_pin = locksmith::MergeInput::from(project);
    let manifest = json!({"packageManager":"pnpm@12.21.0"});
    let raw = json!({"projectScopedOptionalPeers":true});
    let overridden = json!({"projectScopedOptionalPeers":true,"packageManager":"pnpm@12.19.0"});
    let final_version = json!({"pmOnFail":"ignore"});
    let (branches, _, _) = MergeTarget::Ours
        .workspace_configurations(
            Some(&manifest),
            [source, source, no_pin],
            [&raw, &overridden, &raw],
            &final_version,
        )
        .unwrap();
    let expected = ["12.18.0", "12.19.0", "12.21.0"].map(|version| Some(version.parse().unwrap()));
    assert_eq!(branches.map(|config| config.pnpm_version), expected);
}

#[test]
fn the_selected_bootstrap_manager_controls_the_final_project_settings() {
    let make = |pnpm: &str, tool: &str, scoped: bool| {
        let env = json!({"lockfileVersion":"9.0","importers":{".":{
            "configDependencies":{"tool":{"specifier":tool,"version":tool}},
            "packageManagerDependencies":{"pnpm":{"specifier":"^12.18.0","version":pnpm}}}},
            "packages":{format!("tool@{tool}"):{"resolution":{}},format!("pnpm@{pnpm}"):{"resolution":{}}},
            "snapshots":{format!("tool@{tool}"): {},format!("pnpm@{pnpm}"): {}}});
        let mut project = json!({"lockfileVersion":"9.0","importers":{".":{}}});
        if scoped {
            project["settings"] = json!({"projectScopedOptionalPeers":true});
        }
        format!("---\n{env}\n---\n{project}\n")
    };
    let old_yaml = make("12.18.0", "1.0.0", false);
    let new_yaml = make("12.21.0", "2.0.0", true);
    let old = Lockfile::parse(&old_yaml).unwrap();
    let new = Lockfile::parse(&new_yaml).unwrap();
    let before = json!({"configDependencies":{"tool":"1.0.0"}});
    let after = json!({"configDependencies":{"tool":"2.0.0"},"projectScopedOptionalPeers":true});
    // Duplicate snapshot values agree, so the bootstrap and its version remain usable.
    let snapshot = r#""pnpm@12.21.0":{}"#;
    let duplicate = new_yaml.replace(snapshot, &format!("{snapshot},{snapshot}"));
    let inputs = [
        locksmith::MergeInput::from(old_yaml.as_str()),
        (&old).into(),
        duplicate.as_str().into(),
    ];
    for root in [
        json!({}),
        json!({"devEngines":{"packageManager":{"name":"pnpm","version":"^12.18.0"}}}),
    ] {
        let manifests = BTreeMap::from([(".".into(), root.clone())]);
        for (wanted, expected) in [(&after, &new), (&before, &old)] {
            let (branches, final_config, prepared) = MergeTarget::Ours
                .workspace_configurations(Some(&root), inputs, [&before, &before, &after], wanted)
                .unwrap();
            let context = MergeContext {
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: branches.each_ref(),
                configuration: &final_config,
                patches: &BTreeMap::new(),
                registry: None,
            };
            let result = prepared.merge(&context).unwrap();
            assert_eq!(result.environment(), expected.environment());
            if expected.recorded_pnpm_version() != old.recorded_pnpm_version() {
                let wrong_runner = WorkspaceConfig::from_pnpm_version(
                    wanted,
                    &old.recorded_pnpm_version().unwrap(),
                )
                .unwrap();
                let incompatible = MergeContext {
                    configuration: &wrong_runner,
                    ..context
                };
                assert!(merge(&old, &old, &new, Some(&incompatible)).is_err());
            }
            let version = result.recorded_pnpm_version().unwrap();
            let native = WorkspaceConfig::from_pnpm_version(wanted, &version).unwrap();
            result
                .validate_workspace(
                    &manifests,
                    &Default::default(),
                    &native,
                    &BTreeMap::new(),
                    |_| Ok(None),
                )
                .unwrap();
        }
    }
}

fn check_manager_pin(
    manifest: serde_json::Value,
    version: &str,
    settings: serde_json::Value,
    recorded: Option<(&str, &str)>,
    valid: bool,
) {
    let config = WorkspaceConfig::from_pnpm_version(&settings, &version.parse().unwrap()).unwrap();
    let manifests = BTreeMap::from([(".".into(), manifest)]);
    let patches = BTreeMap::new();
    let ctx = MergeContext {
        target: MergeTarget::Ours,
        manifests: &manifests,
        configurations: [&config; 3],
        configuration: &config,
        patches: &patches,
        registry: None,
    };
    let repair = locksmith::RepairContext {
        manifests: &manifests,
        configuration: &config,
        patches: &patches,
        parents: &[],
        registry: None,
    };
    let root = &manifests["."];
    let changes = [locksmith::ManifestChange {
        importer: ".",
        before: Some(root),
        after: Some(root),
    }];
    let options = locksmith::ConvergeOptions {
        baseline_configuration: Some(&config),
        configuration: Some(&config),
        ..Default::default()
    };
    let project = "lockfileVersion: '9.0'\nimporters: {.: {}}\n";
    let yaml = match recorded {
        None => project.to_owned(),
        Some((specifier, resolved)) => {
            let env = json!({"lockfileVersion":"9.0","importers":{".":{"configDependencies":{},"packageManagerDependencies":{"pnpm":{"specifier":specifier,"version":resolved}}}},
                "packages":{format!("pnpm@{resolved}"):{"resolution":{}}},"snapshots":{format!("pnpm@{resolved}"): {}}});
            format!("---\n{env}\n---\n{project}")
        }
    };
    let lock = Lockfile::parse(&yaml).unwrap();
    assert_eq!(
        lock.validate_workspace(&manifests, &Default::default(), &config, &patches, |_| Ok(
            None
        ))
        .is_ok(),
        valid,
        "validate {root} {settings}"
    );
    assert_eq!(
        locksmith::repair(&yaml, &repair).is_ok(),
        valid,
        "repair {root} {settings}"
    );
    assert_eq!(
        merge(&lock, &lock, &lock, Some(&ctx)).is_ok(),
        valid,
        "merge {root} {settings}"
    );
    assert_eq!(
        locksmith::converge(&lock, &lock, &changes, options).is_ok(),
        valid,
        "converge {root} {settings}"
    );
}

#[test]
fn workspace_operations_check_the_recorded_package_manager_pin() {
    let version = "12.21.0";
    let manifest = json!({"packageManager":format!("pnpm@{version}+sha512.abc")});
    for (specifier, resolved, valid) in [
        (version, version, true),
        ("12.20.0", "12.20.0", false),
        (version, "12.20.0", false),
        ("12.20.0", version, false),
    ] {
        check_manager_pin(
            manifest.clone(),
            version,
            json!({}),
            Some((specifier, resolved)),
            valid,
        );
    }
}

#[test]
fn the_package_manager_policy_determines_whether_a_bootstrap_pin_is_required() {
    for (version, required) in [("10.34.9", false), ("11.28.12", false), ("12.21.0", true)] {
        let manifest = json!({"packageManager":format!("pnpm@{version}")});
        check_manager_pin(manifest.clone(), version, json!({}), None, !required);
        check_manager_pin(manifest, version, json!({"pmOnFail":"ignore"}), None, true);
    }
}

#[test]
fn dev_engines_selects_the_bootstrap_package_manager_and_its_policy() {
    let spec = "^12.20.0";
    let engine = json!({"name":"pnpm","version":spec,"onFail":"download"});
    let manifest = json!({"packageManager":"pnpm@11.28.12","devEngines":{"packageManager":engine}});
    for (recorded, valid) in [
        (None, false),
        (Some((spec, "12.21.0")), true),
        (Some(("12.21.0", "12.21.0")), true),
        (Some(("11.28.12", "11.28.12")), false),
    ] {
        check_manager_pin(manifest.clone(), "12.21.0", json!({}), recorded, valid);
    }
    let ignored = json!({"devEngines":{"packageManager":{"name":"pnpm","version":"11.28.12","onFail":"ignore"}}});
    let managed = json!({"devEngines":{"packageManager":{"name":"pnpm","version":"11.28.12"}}});
    // A pnpm 11 bootstrap pin also installs @pnpm/exe.
    check_manager_pin(
        managed,
        "11.28.12",
        json!({}),
        Some(("11.28.12", "11.28.12")),
        false,
    );
    check_manager_pin(ignored.clone(), "11.28.12", json!({}), None, true);
    check_manager_pin(
        ignored.clone(),
        "11.28.12",
        json!({}),
        Some(("11.27.0", "11.27.0")),
        true,
    );
    check_manager_pin(
        ignored,
        "11.28.12",
        json!({"pmOnFail":"download"}),
        None,
        false,
    );
    let pnpm = json!({"name":"pnpm","version":"12.21.0"});
    let npm = json!({"name":"npm","version":"11.0.0"});
    for (engines, valid) in [(json!([pnpm, npm]), true), (json!([npm, pnpm]), false)] {
        check_manager_pin(
            json!({"devEngines":{"packageManager":engines}}),
            "12.21.0",
            json!({}),
            None,
            valid,
        );
    }
}

#[test]
fn root_importer_peers_apply_to_importers_sorting_before_dot() {
    let make = |peer_version: &str, lib_pin: &str| {
        let mut doc = json!({
            "lockfileVersion": "9.0",
            "importers": {
                "!app": {"dependencies": {"lib": {"specifier": "1.0.0", "version": lib_pin}}},
                ".": {"dependencies": {"peer": {"specifier": peer_version, "version": peer_version}}}
            },
            "packages": {
                "lib@1.0.0": {"resolution": {"integrity": "sha512-lib"}, "peerDependencies": {"peer": "^1 || ^2"}},
                format!("peer@{peer_version}"): {"resolution": {"integrity": "sha512-peer"}}
            },
            "snapshots": {
                format!("lib@1.0.0(peer@{peer_version})"): {"dependencies": {"peer": peer_version}},
                format!("peer@{peer_version}"): {}
            }
        });
        doc["importers"]
            .as_object_mut()
            .unwrap()
            .get_mut("!app")
            .unwrap()["dependencies"]["lib"]["version"] = json!(lib_pin);
        Lockfile::parse(&doc.to_string()).unwrap()
    };
    let base = make("1.0.0", "1.0.0(peer@1.0.0)");
    let ours = make("2.0.0", "1.0.0(peer@2.0.0)");

    let manifests = BTreeMap::from([
        ("!app".into(), json!({"dependencies": {"lib": "1.0.0"}})),
        (".".into(), json!({"dependencies": {"peer": "2.0.0"}})),
    ]);
    let config = WorkspaceConfig::default();
    let merged = merge(
        &base,
        &ours,
        &base,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    let doc = merged.project().unwrap();
    assert_eq!(
        doc["importers"]["!app"]["dependencies"]["lib"]["version"],
        "1.0.0(peer@2.0.0)"
    );
    let keys: Vec<_> = doc["importers"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["!app", "."]);
}

#[test]
fn failing_all_bootstraps_returns_target_branch_error() {
    let make = |version: &str| {
        format!(
            "lockfileVersion: '9.0'\nimporters:\n  .:\n    packageManagerDependencies:\n      pnpm:\n        specifier: {version}\n        version: {version}\n---\nlockfileVersion: '9.0'\nimporters:\n  .: {{}}\n"
        )
    };
    let base = make("10.0.0");
    let ours = make("11.0.0");
    let theirs = make("12.0.0");
    let inputs = [
        locksmith::MergeInput::from(base.as_str()),
        ours.as_str().into(),
        theirs.as_str().into(),
    ];
    for (target, expected) in [
        (MergeTarget::Ours, "ours-err: 11.0.0"),
        (MergeTarget::Theirs, "theirs-err: 12.0.0"),
    ] {
        let mut attempted = Vec::new();
        let err = target
            .workspace_for_candidate(
                None,
                inputs,
                |lock| -> Result<((), WorkspaceConfig), LockfileError> {
                    let ver = lock
                        .and_then(Lockfile::recorded_pnpm_version)
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    let label = match ver.as_str() {
                        "10.0.0" => "base-err",
                        "11.0.0" => "ours-err",
                        "12.0.0" => "theirs-err",
                        _ => "unknown",
                    };
                    let error = LockfileError::WorkspaceInput(format!("{label}: {ver}"));
                    attempted.push(ver);
                    Err(error)
                },
            )
            .err()
            .unwrap();
        attempted.sort_unstable();
        assert_eq!(attempted, ["10.0.0", "11.0.0", "12.0.0"]);
        assert!(
            matches!(&err, LockfileError::WorkspaceInput(msg) if msg == expected),
            "expected {expected}, got {err}"
        );
    }
}
