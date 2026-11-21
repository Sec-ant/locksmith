use super::{cfg, lock};
use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeTarget, PeerRules, Value, WorkspaceConfig,
    inspect_peers, merge,
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
            registry: None,
            target: MergeTarget::Ours,
            manifests: &BTreeMap::from([(".".into(), json!({}))]),
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
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
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
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
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
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
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&normalized_config; 3],
            configuration: &normalized_config,
            patches: &patches,
        };
        let result = merge(&base, &ours, &theirs, Some(&context)).unwrap();
        result.validate().unwrap();
        let swapped = MergeContext {
            registry: None,
            target: MergeTarget::Ours,
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
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
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
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_old, &normalized_new, &normalized_old],
        configuration: &normalized_new,
        patches: &patches,
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
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
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
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_before; 3],
        configuration: &normalized_after,
        patches: &patches,
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
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
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
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &BTreeMap::new(),
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
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&cfg(&before); 3],
                configuration: &cfg(&after),
                patches: &patches
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
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&config); 3],
            configuration: &cfg(&config),
            patches: &patches,
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
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&before, &after, &before],
                configuration: &after,
                patches: &BTreeMap::new(),
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
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&before, &before, &after],
            configuration: &after,
            patches: &patches,
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
            registry: None,
            target: MergeTarget::Ours,
            manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &patches,
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
            registry: None,
            manifests: &manifests,
            target: MergeTarget::Ours,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
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
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
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
                registry: None,
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &BTreeMap::new(),
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
                registry: None,
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&baseline, &baseline, &config],
                configuration: &config,
                patches: &BTreeMap::new(),
            }),
        )
        .unwrap();
        assert_eq!(
            merged.project().unwrap()["importers"]["."]["dependencies"]["foo"]["version"],
            expected
        );
    }
}
