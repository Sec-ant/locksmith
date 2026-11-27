use super::{cfg, lock};
use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeTarget, PeerRules, Value, inspect_peers, merge,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn catalog_overrides_are_written_as_effective_specifiers() {
    let base = lock(1, 1, false);
    let new = lock(2, 1, false);
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"catalog:","b":"*","lib":"1.0.0"}}),
    )]);
    let old = json!({});
    let config = json!({"catalog":{"a":"2.0.0"},"overrides":{"lib>a":"catalog:"}});
    let patches = BTreeMap::new();
    let normalized_old = cfg(&old);
    let normalized_config = cfg(&config);
    let context = MergeContext {
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_old; 3],
        configuration: &normalized_config,
        patches: &patches,
    };
    let output = merge(&base, &new, &base, Some(&context)).unwrap();
    assert_eq!(output.project().unwrap()["overrides"]["lib>a"], "2.0.0");
}

#[test]
fn changed_peer_overrides_rebuild_declarations_and_instances() {
    use sha2::{Digest, Sha256};
    for (explicit_provider, opaque) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let make = |release: u8, provider: u8| {
            let suffix = if opaque {
                "(ffffffffffffffffffffffffffffffff)".to_owned()
            } else {
                format!("(a@{provider}.0.0)")
            };
            let mut data = json!({
                "lockfileVersion":"9.0",
                "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
                "overrides":{"lib>a":format!("{provider}.0.0")},
                "importers":{".":{"dependencies":{"lib":{"specifier":format!("{release}.0.0"),"version":format!("{release}.0.0{suffix}")}}}},
                "packages":{
                    format!("lib@{release}.0.0"):{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"a":format!("{provider}.0.0")}},
                    "a@1.0.0":{"resolution":{"integrity":"sha512-a1"}},
                    "a@2.0.0":{"resolution":{"integrity":"sha512-a2"}}
                },
                "snapshots":{
                    format!("lib@{release}.0.0{suffix}"):{"dependencies":{"a":format!("{provider}.0.0")}},
                    "a@1.0.0":{},"a@2.0.0":{}
                }
            });
            if explicit_provider {
                data["importers"]["."]["dependencies"]["a"] =
                    json!({"specifier":"1.0.0","version":"1.0.0"});
            }
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let before = cfg(&json!({"overrides":{"lib>a":"1.0.0"}}));
        let after = cfg(&json!({"overrides":{"lib>a":"2.0.0"}}));
        let mut manifest = json!({"dependencies":{"lib":"2.0.0"}});
        if explicit_provider {
            manifest["dependencies"]["a"] = json!("1.0.0");
        }
        let manifests = BTreeMap::from([(".".into(), manifest)]);
        let result = merge(
            &make(1, 1),
            &make(1, 2),
            &make(2, 1),
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
        let provider = if explicit_provider { 1 } else { 2 };
        let reference = if opaque {
            let hash: String = Sha256::digest(format!("a@{provider}.0.0").as_bytes())[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("2.0.0({hash})")
        } else {
            format!("2.0.0(a@{provider}.0.0)")
        };
        let data = result.project().unwrap();
        assert_eq!(
            data["packages"]["lib@2.0.0"]["peerDependencies"]["a"],
            "2.0.0"
        );
        assert_eq!(
            data["importers"]["."]["dependencies"]["lib"]["version"],
            reference
        );
        assert_eq!(
            data["snapshots"][format!("lib@{reference}")]["dependencies"]["a"],
            format!("{provider}.0.0")
        );
        result.validate().unwrap();
    }
}

#[test]
fn transitive_override_rebinds_sibling_peer_consumers() {
    let make = |a: u8, include_parent: bool| {
        let mut data = lock(a, 1, false).project().unwrap().clone();
        if include_parent {
            data["packages"]["parent@1.0.0"] = json!({"resolution":{"integrity":"sha512-parent"}});
            data["snapshots"]["parent@1.0.0"] = json!({"dependencies":{"a":format!("{a}.0.0"),"b":"1.0.0","lib":format!("1.0.0(a@{a}.0.0)(b@1.0.0)")}});
            data["importers"]["."] =
                json!({"dependencies":{"parent":{"specifier":"1.0.0","version":"1.0.0"}}});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, true);
    let ours = make(2, false);
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"parent":"1.0.0"}}))]);
    let before = json!({});
    let after = json!({"overrides":{"parent@1>a":"2.0.0"}});
    let patches = BTreeMap::new();
    let output = merge(
        &base,
        &ours,
        &base,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before); 3],
            configuration: &cfg(&after),
            patches: &patches,
        }),
    )
    .unwrap();
    let parent = &output.project().unwrap()["snapshots"]["parent@1.0.0"]["dependencies"];
    assert_eq!(parent["a"], "2.0.0");
    assert_eq!(parent["lib"], "1.0.0(a@2.0.0)(b@1.0.0)");
    output.validate().unwrap();
}

#[test]
fn changed_qualified_transitive_overrides_require_the_original_declaration() {
    let mut value = lock(1, 1, false).project().unwrap().clone();
    value["packages"]["parent@1.0.0"] = json!({"resolution":{"integrity":"sha512-parent"}});
    value["snapshots"]["parent@1.0.0"] = json!({"dependencies":{"a":"1.0.0"}});
    value["importers"]["."] =
        json!({"dependencies":{"parent":{"specifier":"1.0.0","version":"1.0.0"}}});
    let input = Lockfile::parse(&value.to_string()).unwrap();
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"parent":"1.0.0"}}))]);
    let before = json!({});
    let after = json!({"overrides":{"a@^1":"1.0.0"}});
    let patches = BTreeMap::new();
    let error = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before); 3],
            configuration: &cfg(&after),
            patches: &patches,
        }),
    )
    .unwrap_err();
    assert!(
        matches!(error,LockfileError::ResolutionRequired {path,..} if path==["snapshots","parent@1.0.0","a"])
    );
}

#[test]
fn a_failed_transitive_choice_keeps_the_target_pin_for_unrelated_dependencies() {
    let make = |version: &str, has_child: bool| {
        let mut doc = json!({
            "lockfileVersion": "9.0",
            "settings": {"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers": {".": {"dependencies": {
                "parent": {"specifier":"*","version":version},
                "other": {"specifier":"*","version":version}
            }}},
            "packages": {
                format!("parent@{version}"): {"resolution":{"integrity":"sha512-parent"}},
                format!("other@{version}"): {"resolution":{"integrity":"sha512-other"}}
            },
            "snapshots": {format!("parent@{version}"): {}, format!("other@{version}"): {}}
        });
        if has_child {
            doc["snapshots"][format!("parent@{version}")]["dependencies"] =
                json!({"child":"1.0.0"});
            doc["packages"]["child@1.0.0"] = json!({"resolution":{"integrity":"sha512-child"}});
            doc["snapshots"]["child@1.0.0"] = json!({});
        }
        Lockfile::parse(&doc.to_string()).unwrap()
    };
    let ours = make("1.0.0", true);
    let theirs = make("2.0.0", false);
    let before = cfg(&json!({}));
    let after = cfg(&json!({"overrides":{"child@^1":"1.0.1"}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"parent":"*","other":"*"}}),
    )]);
    let output = merge(
        &ours,
        &ours,
        &theirs,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            configurations: [&before; 3],
            configuration: &after,
            manifests: &manifests,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    output.validate().unwrap();
    let pins = &output.project().unwrap()["importers"]["."]["dependencies"];
    assert_eq!(pins["parent"]["version"], "2.0.0");
    assert_eq!(pins["other"]["version"], "1.0.0");
}

#[test]
fn qualified_override_changes_reuse_a_resolution_proven_in_another_context() {
    let make = |provider, runtime, overrides: Value| {
        let mut data = lock(provider, 1, false).project().unwrap().clone();
        data["overrides"] = overrides;
        data["packages"][format!("runtime@{runtime}.0.0")] =
            json!({"resolution":{"integrity":"sha512-runtime"}});
        data["snapshots"][format!("runtime@{runtime}.0.0")] = json!({});
        data["snapshots"][format!("lib@1.0.0(a@{provider}.0.0)(b@1.0.0)")]["dependencies"]["runtime"] =
            json!(format!("{runtime}.0.0"));
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = json!({"overrides":{"runtime":"2.0.0"}});
    let after = json!({"overrides":{"runtime@1":"2.0.0"}});
    let base = make(1, 2, before["overrides"].clone());
    let ours = make(2, 2, after["overrides"].clone());
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"1.0.0","b":"1.0.0","lib":"1.0.0"}}),
    )]);
    let patches = BTreeMap::new();
    let result = merge(
        &base,
        &ours,
        &base,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before), &cfg(&after), &cfg(&before)],
            configuration: &cfg(&after),
            patches: &patches,
        }),
    )
    .unwrap();
    result.validate().unwrap();
    assert_eq!(
        result.project().unwrap()["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["runtime"],
        "2.0.0"
    );
    assert_eq!(result.project().unwrap()["overrides"], after["overrides"]);

    let upgraded = json!({"overrides":{"runtime@1":"3.0.0"}});
    let upgraded_lock = make(2, 3, upgraded["overrides"].clone());
    let result = merge(
        &base,
        &upgraded_lock,
        &base,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before), &cfg(&upgraded), &cfg(&before)],
            configuration: &cfg(&upgraded),
            patches: &patches,
        }),
    )
    .unwrap();
    result.validate().unwrap();
    assert_eq!(
        result.project().unwrap()["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["runtime"],
        "3.0.0"
    );

    let mut changed_artifact = ours.project().unwrap().clone();
    changed_artifact["packages"]["lib@1.0.0"]["resolution"]["integrity"] =
        json!("sha512-different-content");
    let changed_artifact = Lockfile::parse(&changed_artifact.to_string()).unwrap();
    assert!(matches!(
        merge(
            &base,
            &changed_artifact,
            &base,
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&cfg(&before), &cfg(&after), &cfg(&before)],
                configuration: &cfg(&after),
                patches: &patches,
            })
        ),
        Err(LockfileError::ResolutionRequired { .. })
    ));
}

#[test]
fn reordered_overrides_select_the_final_resolution_and_survive_rendering() {
    let before = json!({"overrides":{"runtime@^1":"2.0.0","runtime@1":"3.0.0"}});
    let after = json!({"overrides":{"runtime@1":"3.0.0","runtime@^1":"2.0.0"}});
    let make = |rules: &Value, runtime: &str| {
        Lockfile::parse(&json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "overrides": rules["overrides"],
            "importers":{".":{"dependencies":{"lib":{"specifier":"1.0.0","version":"1.0.0"}}}},
            "packages":{
                "lib@1.0.0":{"resolution":{"integrity":"sha512-lib"}},
                format!("runtime@{runtime}"):{"resolution":{"integrity":"sha512-runtime"}}
            },
            "snapshots":{"lib@1.0.0":{"dependencies":{"runtime":runtime}},format!("runtime@{runtime}"):{}}
        }).to_string()).unwrap()
    };
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"lib":"1.0.0"}}))]);
    let patches = BTreeMap::new();
    for previous in ["3.0.0", "2.0.0"] {
        let base = make(&before, previous);
        let ours = make(&after, "2.0.0");
        let result = merge(
            &base,
            &ours,
            &base,
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&cfg(&before), &cfg(&after), &cfg(&before)],
                configuration: &cfg(&after),
                patches: &patches,
            }),
        )
        .unwrap();
        assert_eq!(
            result.project().unwrap()["snapshots"]["lib@1.0.0"]["dependencies"]["runtime"],
            "2.0.0"
        );
        let rendered = Lockfile::parse(&result.to_yaml().unwrap()).unwrap();
        assert_eq!(
            cfg(rendered.project().unwrap()).overrides,
            cfg(&after).overrides
        );
    }
}

#[test]
fn reordering_rules_for_unrelated_names_preserves_the_target_resolution() {
    let make = |runtime: &str, overrides: Value| {
        let mut data = lock(1, 1, false).project().unwrap().clone();
        data["overrides"] = overrides;
        data["packages"][format!("runtime@{runtime}")] =
            json!({"resolution":{"integrity":"sha512-runtime"}});
        data["snapshots"][format!("runtime@{runtime}")] = json!({});
        data["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["runtime"] =
            json!(runtime);
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = json!({"overrides":{"x":"1.0.0","y":"1.0.0","left>runtime":"1.0.0","right>runtime":"2.0.0"}});
    let after = json!({"overrides":{"y":"1.0.0","x":"1.0.0","right>runtime":"2.0.0","left>runtime":"1.0.0"}});
    let base = make("1.0.0", before["overrides"].clone());
    let ours = make("2.0.0", after["overrides"].clone());
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"*","b":"*","lib":"1.0.0"}}),
    )]);
    let patches = BTreeMap::new();
    let result = merge(
        &base,
        &ours,
        &base,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&cfg(&before), &cfg(&after), &cfg(&before)],
            configuration: &cfg(&after),
            patches: &patches,
        }),
    )
    .unwrap();
    assert_eq!(
        result.project().unwrap()["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["runtime"],
        "1.0.0"
    );
    let rendered = Lockfile::try_from(result.to_yaml().unwrap()).unwrap();
    assert_eq!(rendered.project(), base.project());
    assert!(
        rendered.project().unwrap()["overrides"]
            .as_object()
            .unwrap()
            .keys()
            .eq(after["overrides"].as_object().unwrap().keys())
    );
}

#[test]
fn override_upgrades_reuse_parent_instances_with_new_transitive_peers() {
    let make = |provider: u8, upgraded: bool| {
        let suffix = if upgraded {
            format!("(a@{provider}.0.0)")
        } else {
            String::new()
        };
        let runtime = if upgraded { "2.0.0" } else { "1.0.0" };
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "overrides":{"runtime":runtime},
            "importers":{".":{"dependencies":{
                "a":{"specifier":"*","version":format!("{provider}.0.0")},
                "lib":{"specifier":"1.0.0","version":format!("1.0.0{suffix}")}
            }}},
            "packages":{
                format!("a@{provider}.0.0"):{"resolution":{"integrity":"sha512-a"}},
                "lib@1.0.0":{"resolution":{"integrity":"sha512-lib"}},
                "stable@1.0.0":{"resolution":{"integrity":"sha512-stable"}},
                format!("runtime@{runtime}"):{"resolution":{"integrity":"sha512-runtime"}}
            },
            "snapshots":{
                format!("a@{provider}.0.0"): {}, "stable@1.0.0":{},
                format!("lib@1.0.0{suffix}"):{"dependencies":{"runtime":format!("{runtime}{suffix}"),"stable":"1.0.0"}},
                format!("runtime@{runtime}{suffix}"):{}
            }
        });
        if upgraded {
            data["packages"]["runtime@2.0.0"]["peerDependencies"] = json!({"a":"*"});
            data["snapshots"][format!("runtime@2.0.0{suffix}")]["dependencies"] =
                json!({"a":format!("{provider}.0.0")});
            data["snapshots"][format!("lib@1.0.0{suffix}")]["transitivePeerDependencies"] =
                json!(["a"]);
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, false);
    let ours = make(1, true);
    let theirs = make(2, false);
    let before = cfg(&json!({"overrides":{"runtime":"1.0.0"}}));
    let after = cfg(&json!({"overrides":{"runtime":"2.0.0"}}));
    let final_config = cfg(&json!({"overrides":{"runtime":"2.0.0","unrelated":"3.0.0"}}));
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"a":"*","lib":"1.0.0"}}))]);
    let patches = BTreeMap::new();
    let context = MergeContext {
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&before, &after, &before],
        configuration: &final_config,
        patches: &patches,
    };
    let result = merge(&base, &ours, &theirs, Some(&context)).unwrap();
    let data = result.project().unwrap();
    assert_eq!(
        data["importers"]["."]["dependencies"]["lib"]["version"],
        "1.0.0(a@2.0.0)"
    );
    assert_eq!(
        data["snapshots"]["lib@1.0.0(a@2.0.0)"]["dependencies"],
        json!({"runtime":"2.0.0(a@2.0.0)","stable":"1.0.0"})
    );
    assert_eq!(
        data["snapshots"]["lib@1.0.0(a@2.0.0)"]["transitivePeerDependencies"],
        json!(["a"])
    );
    assert_eq!(
        data["snapshots"]["runtime@2.0.0(a@2.0.0)"]["dependencies"]["a"],
        "2.0.0"
    );
    result.validate().unwrap();

    // A matching version alone does not prove the same artifact or authorize
    // an unrelated transitive upgrade from the witness branch.
    for changed_artifact in [true, false] {
        let mut changed = ours.project().unwrap().clone();
        if changed_artifact {
            changed["packages"]["lib@1.0.0"]["resolution"]["integrity"] = json!("sha512-other");
        } else {
            changed["packages"]["stable@2.0.0"] =
                json!({"resolution":{"integrity":"sha512-stable-two"}});
            changed["snapshots"]["stable@2.0.0"] = json!({});
            changed["snapshots"]["lib@1.0.0(a@1.0.0)"]["dependencies"]["stable"] = json!("2.0.0");
        }
        let changed = Lockfile::parse(&changed.to_string()).unwrap();
        let result = merge(&base, &changed, &theirs, Some(&context)).unwrap();
        let data = result.project().unwrap();
        assert_eq!(
            data["packages"]["lib@1.0.0"],
            theirs.project().unwrap()["packages"]["lib@1.0.0"]
        );
        assert_eq!(
            data["snapshots"]["lib@1.0.0(a@2.0.0)"]["dependencies"],
            json!({"runtime":"2.0.0(a@2.0.0)","stable":"1.0.0"})
        );
    }
}

#[test]
fn new_override_peers_propagate_through_new_parent_releases_and_cycles() {
    for cycle in [false, true] {
        let make = |release: u8, provider: u8, upgraded: bool| {
            let suffix = if upgraded {
                format!("(a@{provider}.0.0)")
            } else {
                String::new()
            };
            let runtime = if upgraded { "2.0.0" } else { "1.0.0" };
            let mut data = json!({
                "lockfileVersion":"9.0",
                "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
                "overrides":{"runtime":runtime},
                "importers":{".":{"dependencies":{
                    "a":{"specifier":"*","version":format!("{provider}.0.0")},
                    "wrapper":{"specifier":format!("{release}.0.0"),"version":format!("{release}.0.0{suffix}")}
                }}},
                "packages":{
                    format!("a@{provider}.0.0"):{"resolution":{"integrity":format!("sha512-a-{provider}")}},
                    format!("wrapper@{release}.0.0"):{"resolution":{"integrity":format!("sha512-wrapper-{release}")}},
                    format!("lib@{release}.0.0"):{"resolution":{"integrity":format!("sha512-lib-{release}")}},
                    format!("runtime@{runtime}"):{"resolution":{"integrity":format!("sha512-runtime-{runtime}")}}
                },
                "snapshots":{
                    format!("a@{provider}.0.0"): {},
                    format!("wrapper@{release}.0.0{suffix}"):{"dependencies":{"lib":format!("{release}.0.0{suffix}")}},
                    format!("lib@{release}.0.0{suffix}"):{"dependencies":{"runtime":format!("{runtime}{suffix}")}},
                    format!("runtime@{runtime}{suffix}"):{}
                }
            });
            if cycle {
                data["snapshots"][format!("lib@{release}.0.0{suffix}")]["dependencies"]["wrapper"] =
                    json!(format!("{release}.0.0{suffix}"));
            }
            for provider in [1, 3] {
                data["packages"][format!("a@{provider}.0.0")] =
                    json!({"resolution":{"integrity":format!("sha512-a-{provider}")}});
                data["snapshots"][format!("a@{provider}.0.0")] = json!({});
            }
            if release == 2 {
                for (id, provider) in [("packages/left", 1), ("packages/right", 3)] {
                    data["importers"][id] = json!({"dependencies":{
                        "a":{"specifier":format!("{provider}.0.0"),"version":format!("{provider}.0.0")},
                        "wrapper":{"specifier":"2.0.0","version":"2.0.0"}
                    }});
                }
            }
            if upgraded {
                data["packages"]["runtime@2.0.0"]["peerDependencies"] = json!({"a":"*"});
                data["snapshots"][format!("runtime@2.0.0{suffix}")]["dependencies"] =
                    json!({"a":format!("{provider}.0.0")});
                for name in ["wrapper", "lib"] {
                    data["snapshots"][format!("{name}@{release}.0.0{suffix}")]["transitivePeerDependencies"] =
                        json!(["a"]);
                }
            }
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let base = make(1, 1, false);
        let ours = make(1, 1, true);
        let theirs = make(2, 2, false);
        let before = cfg(&json!({"overrides":{"runtime":"1.0.0"}}));
        let after = cfg(&json!({"overrides":{"runtime":"2.0.0"}}));
        let mut manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"a":"*","wrapper":"2.0.0"}}),
        )]);
        for (id, provider) in [("packages/left", 1), ("packages/right", 3)] {
            manifests.insert(
                id.into(),
                json!({"dependencies":{"a":format!("{provider}.0.0"),"wrapper":"2.0.0"}}),
            );
        }
        let patches = BTreeMap::new();
        let context = MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&before, &after, &before],
            configuration: &after,
            patches: &patches,
        };
        let result = merge(&base, &ours, &theirs, Some(&context)).unwrap();
        let data = result.project().unwrap();
        assert_eq!(
            data["importers"]["."]["dependencies"]["wrapper"]["version"],
            "2.0.0(a@2.0.0)"
        );
        for name in ["wrapper", "lib"] {
            assert_eq!(
                data["packages"][format!("{name}@2.0.0")],
                theirs.project().unwrap()["packages"][format!("{name}@2.0.0")]
            );
            assert_eq!(
                data["snapshots"][format!("{name}@2.0.0(a@2.0.0)")]["transitivePeerDependencies"],
                json!(["a"])
            );
        }
        assert_eq!(
            data["snapshots"]["wrapper@2.0.0(a@2.0.0)"]["dependencies"]["lib"],
            "2.0.0(a@2.0.0)"
        );
        assert_eq!(
            data["snapshots"]["lib@2.0.0(a@2.0.0)"]["dependencies"]["runtime"],
            "2.0.0(a@2.0.0)"
        );
        assert_eq!(
            data["snapshots"]["runtime@2.0.0(a@2.0.0)"]["dependencies"]["a"],
            "2.0.0"
        );
        if cycle {
            assert_eq!(
                data["snapshots"]["lib@2.0.0(a@2.0.0)"]["dependencies"]["wrapper"],
                "2.0.0(a@2.0.0)"
            );
        }
        for (id, provider) in [("packages/left", 1), ("packages/right", 3)] {
            assert_eq!(
                data["importers"][id]["dependencies"]["wrapper"]["version"],
                format!("2.0.0(a@{provider}.0.0)")
            );
            assert_eq!(
                data["snapshots"][format!("lib@2.0.0(a@{provider}.0.0)")]["dependencies"]["runtime"],
                format!("2.0.0(a@{provider}.0.0)")
            );
            assert_eq!(
                data["snapshots"][format!("runtime@2.0.0(a@{provider}.0.0)")]["dependencies"]["a"],
                format!("{provider}.0.0")
            );
        }
        result.validate().unwrap();
        let repeated = merge(
            &result,
            &result,
            &result,
            Some(&MergeContext {
                registry: None,
                configurations: [&after; 3],
                ..context
            }),
        )
        .unwrap();
        assert_eq!(repeated.to_yaml().unwrap(), result.to_yaml().unwrap());
        let mut shadowed = theirs.project().unwrap().clone();
        shadowed["snapshots"]["wrapper@2.0.0"]["dependencies"]["a"] = json!("1.0.0");
        let shadowed = Lockfile::parse(&shadowed.to_string()).unwrap();
        let result = merge(&base, &ours, &shadowed, Some(&context)).unwrap();
        let data = result.project().unwrap();
        for id in [".", "packages/left", "packages/right"] {
            assert_eq!(
                data["importers"][id]["dependencies"]["wrapper"]["version"],
                "2.0.0"
            );
        }
        assert_eq!(
            data["snapshots"]["wrapper@2.0.0"]["dependencies"]["lib"],
            "2.0.0(a@1.0.0)"
        );
        assert_eq!(
            data["snapshots"]["runtime@2.0.0(a@1.0.0)"]["dependencies"]["a"],
            "1.0.0"
        );
        result.validate().unwrap();
        let mut linked_manifests = manifests.clone();
        linked_manifests.get_mut(".").unwrap()["dependencies"]["a"] = json!("workspace:*");
        linked_manifests.insert("providers/a".into(), json!({"name":"a","version":"2.0.0"}));
        let linked = merge(
            &base,
            &ours,
            &theirs,
            Some(&MergeContext {
                registry: None,
                manifests: &linked_manifests,
                ..context
            }),
        )
        .unwrap();
        linked.validate().unwrap();
        assert_eq!(
            linked.project().unwrap()["importers"]["."]["dependencies"]["wrapper"]["version"],
            "2.0.0(a@providers+a)"
        );
        assert_eq!(
            linked.project().unwrap()["snapshots"]["runtime@2.0.0(a@providers+a)"]["dependencies"]
                ["a"],
            "link:providers/a"
        );

        let nested = |lock: &Lockfile| {
            let mut data = lock.project().unwrap().clone();
            data["settings"]["dedupePeers"] = json!(true);
            data["packages"]["b@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-b"},"peerDependencies":{"c":"*"}});
            data["packages"]["c@1.0.0"] = json!({"resolution":{"integrity":"sha512-c"}});
            data["snapshots"]["b@1.0.0(c@1.0.0)"] = json!({"dependencies":{"c":"1.0.0"}});
            data["snapshots"]["c@1.0.0"] = json!({});
            let keys: Vec<_> = data["snapshots"]
                .as_object()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with("wrapper@") || key.starts_with("lib@"))
                .cloned()
                .collect();
            for key in keys {
                let mut body = data["snapshots"]
                    .as_object_mut()
                    .unwrap()
                    .remove(&key)
                    .unwrap();
                for name in ["wrapper", "lib"] {
                    if let Some(value) = body["dependencies"].get(name).and_then(Value::as_str) {
                        let value = format!("{value}(b@1.0.0)");
                        body["dependencies"][name] = json!(value);
                    }
                }
                if key.starts_with("lib@") {
                    data["packages"][key.split('(').next().unwrap()]["peerDependencies"] =
                        json!({"b":"*"});
                    body["dependencies"]["b"] = json!("1.0.0(c@1.0.0)");
                } else {
                    let peers = body
                        .as_object_mut()
                        .unwrap()
                        .entry("transitivePeerDependencies")
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .unwrap();
                    peers.push(json!("b"));
                }
                data["snapshots"][format!("{key}(b@1.0.0)")] = body;
            }
            for importer in data["importers"].as_object_mut().unwrap().values_mut() {
                let reference = importer["dependencies"]["wrapper"]["version"]
                    .as_str()
                    .unwrap();
                importer["dependencies"]["wrapper"]["version"] =
                    json!(format!("{reference}(b@1.0.0)"));
            }
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let before_nested = cfg(&json!({"overrides":{"runtime":"1.0.0"},"dedupePeers":true}));
        let after_nested = cfg(&json!({"overrides":{"runtime":"2.0.0"},"dedupePeers":true}));
        let result = merge(
            &nested(&base),
            &nested(&ours),
            &nested(&theirs),
            Some(&MergeContext {
                registry: None,
                configurations: [&before_nested, &after_nested, &before_nested],
                configuration: &after_nested,
                ..context
            }),
        )
        .unwrap();
        let data = result.project().unwrap();
        assert_eq!(
            data["snapshots"]["lib@2.0.0(a@2.0.0)(b@1.0.0)"]["dependencies"]["b"],
            "1.0.0(c@1.0.0)"
        );
        result.validate().unwrap();
        for (optional, auto_install, dedupe, project_scoped, bound) in [
            (true, true, true, false, true),
            (true, true, false, false, false),
            (true, true, true, true, false),
            (false, false, false, false, false),
            (false, true, false, false, true),
            (false, false, true, false, true),
        ] {
            let adjust = |lock: &Lockfile| {
                let mut data = lock.project().unwrap().clone();
                data["settings"]["autoInstallPeers"] = json!(auto_install);
                if project_scoped {
                    data["settings"]["projectScopedOptionalPeers"] = json!(true);
                }
                if optional && data["packages"].get("runtime@2.0.0").is_some() {
                    data["packages"]["runtime@2.0.0"]["peerDependenciesMeta"] =
                        json!({"a":{"optional":true}});
                }
                Lockfile::parse(&data.to_string()).unwrap()
            };
            let source_config = |version| {
                cfg(&json!({
                    "overrides":{"runtime":version},"autoInstallPeers":auto_install,
                    "dedupePeerDependents":dedupe,"projectScopedOptionalPeers":project_scoped
                }))
            };
            let before = source_config("1.0.0");
            let after = source_config("2.0.0");
            let mut manifests = manifests.clone();
            manifests.get_mut(".").unwrap()["dependencies"]
                .as_object_mut()
                .unwrap()
                .remove("a");
            let result = merge(
                &adjust(&base),
                &adjust(&ours),
                &adjust(&theirs),
                Some(&MergeContext {
                    registry: None,
                    target: MergeTarget::Theirs,
                    manifests: &manifests,
                    configurations: [&before, &after, &before],
                    configuration: &after,
                    patches: &patches,
                }),
            )
            .unwrap();
            let data = result.project().unwrap();
            let suffix = if bound { "(a@1.0.0)" } else { "" };
            assert_eq!(
                data["importers"]["."]["dependencies"]["wrapper"]["version"],
                format!("2.0.0{suffix}")
            );
            assert_eq!(
                data["snapshots"][format!("lib@2.0.0{suffix}")]["transitivePeerDependencies"],
                json!(["a"])
            );
            let runtime = &data["snapshots"][format!("runtime@2.0.0{suffix}")];
            if bound {
                assert_eq!(runtime["dependencies"]["a"], "1.0.0");
            } else {
                assert!(
                    runtime
                        .get("dependencies")
                        .and_then(|deps| deps.get("a"))
                        .is_none()
                );
            }
            assert_eq!(
                inspect_peers(&result, &PeerRules::default(), None).is_empty(),
                optional || bound
            );
            result.validate().unwrap();
        }
    }
}

#[test]
fn ordinary_overrides_remove_obsolete_transitive_provider_requirements() {
    let make = |wrapper: u8, package: u8, runtime: u8| {
        let suffix = if runtime == 1 { "(immer@1.0.0)" } else { "" };
        let mut data = json!({
            "lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "overrides":{"package":format!("{package}.0.0"),"runtime":format!("{runtime}.0.0")},
            "importers":{".":{"dependencies":{"wrapper":{"specifier":format!("{wrapper}.0.0"),"version":format!("{wrapper}.0.0{suffix}")}}}},
            "packages":{
                format!("wrapper@{wrapper}.0.0"):{"resolution":{"integrity":"sha512-wrapper"}},
                format!("package@{package}.0.0"):{"resolution":{"integrity":"sha512-package"}},
                format!("runtime@{runtime}.0.0"):{"resolution":{"integrity":"sha512-runtime"}}
            },
            "snapshots":{
                format!("wrapper@{wrapper}.0.0{suffix}"):{"dependencies":{"package":format!("{package}.0.0{suffix}")}},
                format!("package@{package}.0.0{suffix}"):{"dependencies":{"runtime":format!("{runtime}.0.0{suffix}")}},
                format!("runtime@{runtime}.0.0{suffix}"):{}
            }
        });
        if runtime == 1 {
            data["packages"]["runtime@1.0.0"]["peerDependencies"] = json!({"immer":"*"});
            data["snapshots"]["runtime@1.0.0(immer@1.0.0)"]["dependencies"] =
                json!({"immer":"1.0.0"});
            data["packages"]["immer@1.0.0"] = json!({"resolution":{"integrity":"sha512-immer"}});
            data["snapshots"]["immer@1.0.0"] = json!({});
            for key in [
                format!("wrapper@{wrapper}.0.0{suffix}"),
                format!("package@{package}.0.0{suffix}"),
            ] {
                data["snapshots"][key]["transitivePeerDependencies"] = json!(["immer"]);
            }
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let configuration = |package, runtime| {
        cfg(
            &json!({"overrides":{"package":format!("{package}.0.0"),"runtime":format!("{runtime}.0.0")}}),
        )
    };
    let base = configuration(1, 1);
    let ours = configuration(2, 1);
    let theirs = configuration(1, 2);
    let final_config = configuration(2, 2);
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"wrapper":"2.0.0"}}))]);
    let result = merge(
        &make(1, 1, 1),
        &make(1, 2, 1),
        &make(2, 1, 2),
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&base, &ours, &theirs],
            configuration: &final_config,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    let data = result.project().unwrap();
    let wrapper = data["importers"]["."]["dependencies"]["wrapper"]["version"]
        .as_str()
        .unwrap();
    let package = data["snapshots"][format!("wrapper@{wrapper}")]["dependencies"]["package"]
        .as_str()
        .unwrap();
    assert_eq!(
        data["snapshots"][format!("package@{package}")]["dependencies"]["runtime"],
        "2.0.0"
    );
    assert!(data["packages"].get("immer@1.0.0").is_none());
    result.validate().unwrap();
}

#[test]
fn only_effective_direct_catalog_references_have_catalog_snapshots() {
    let locks = lock(1, 1, false);
    let config = cfg(&json!({"catalog":{"a":"1.0.0","b":"1.0.0"},"overrides":{"a":"catalog:"}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"catalog:","b":"catalog:"}}),
    )]);
    let graph = merge(
        &locks,
        &locks,
        &locks,
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
    let doc = graph.project().unwrap();
    assert_eq!(
        doc["importers"]["."]["dependencies"]["a"]["specifier"],
        "1.0.0"
    );
    assert_eq!(
        doc["catalogs"],
        json!({"default":{"b":{"specifier":"1.0.0","version":"1.0.0"}}})
    );
}

#[test]
fn override_repair_tries_all_deferred_candidates_when_earlier_fails() {
    // Both ours and theirs provide `child@2.0.0` with a newly required peer (`extra`),
    // where ours binds `shared` to `1.0.0` (which cannot recontextualize to `shared@2.0.0`
    // in parent's scope) while theirs binds `shared` to `2.0.0`.
    let base = Lockfile::parse(
        &json!({
            "lockfileVersion": "9.0",
            "importers": {".": {"dependencies": {
                "parent": {"specifier": "1.0.0", "version": "1.0.0(shared@2.0.0)"},
                "shared": {"specifier": "2.0.0", "version": "2.0.0"}
            }}},
            "packages": {
                "parent@1.0.0": {"resolution": {"integrity": "sha512-parent"}, "peerDependencies": {"shared": "*"}},
                "child@1.0.0": {"resolution": {"integrity": "sha512-child1"}},
                "shared@2.0.0": {"resolution": {"integrity": "sha512-shared2"}}
            },
            "snapshots": {
                "parent@1.0.0(shared@2.0.0)": {"dependencies": {"child": "1.0.0", "shared": "2.0.0"}},
                "child@1.0.0": {},
                "shared@2.0.0": {}
            }
        })
        .to_string(),
    )
    .unwrap();

    let ours = Lockfile::parse(
        &json!({
            "lockfileVersion": "9.0",
            "overrides": {"parent>child": "2.0.0"},
            "importers": {".": {"dependencies": {
                "parent": {"specifier": "1.0.0", "version": "1.0.0(shared@1.0.0)"},
                "shared": {"specifier": "1.0.0", "version": "1.0.0"}
            }}},
            "packages": {
                "parent@1.0.0": {"resolution": {"integrity": "sha512-parent"}, "peerDependencies": {"shared": "*"}},
                "child@2.0.0": {"resolution": {"integrity": "sha512-child2"}, "peerDependencies": {"extra": "*", "shared": "*"}},
                "extra@1.0.0": {"resolution": {"integrity": "sha512-extra1"}},
                "shared@1.0.0": {"resolution": {"integrity": "sha512-shared1"}}
            },
            "snapshots": {
                "parent@1.0.0(shared@1.0.0)": {"dependencies": {"child": "2.0.0(extra@1.0.0)(shared@1.0.0)", "shared": "1.0.0"}},
                "child@2.0.0(extra@1.0.0)(shared@1.0.0)": {"dependencies": {"extra": "1.0.0", "shared": "1.0.0"}},
                "extra@1.0.0": {},
                "shared@1.0.0": {}
            }
        })
        .to_string(),
    )
    .unwrap();

    let theirs = Lockfile::parse(
        &json!({
            "lockfileVersion": "9.0",
            "overrides": {"parent>child": "2.0.0"},
            "importers": {".": {"dependencies": {
                "parent": {"specifier": "1.0.0", "version": "1.0.0(shared@2.0.0)"},
                "shared": {"specifier": "2.0.0", "version": "2.0.0"}
            }}},
            "packages": {
                "parent@1.0.0": {"resolution": {"integrity": "sha512-parent"}, "peerDependencies": {"shared": "*"}},
                "child@2.0.0": {"resolution": {"integrity": "sha512-child2"}, "peerDependencies": {"extra": "*", "shared": "*"}},
                "extra@2.0.0": {"resolution": {"integrity": "sha512-extra2"}},
                "shared@2.0.0": {"resolution": {"integrity": "sha512-shared2"}}
            },
            "snapshots": {
                "parent@1.0.0(shared@2.0.0)": {"dependencies": {"child": "2.0.0(extra@2.0.0)(shared@2.0.0)", "shared": "2.0.0"}},
                "child@2.0.0(extra@2.0.0)(shared@2.0.0)": {"dependencies": {"extra": "2.0.0", "shared": "2.0.0"}},
                "extra@2.0.0": {},
                "shared@2.0.0": {}
            }
        })
        .to_string(),
    )
    .unwrap();

    let before = cfg(&json!({}));
    let after = cfg(&json!({"overrides": {"parent>child": "2.0.0"}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies": {"parent": "1.0.0", "shared": "2.0.0"}}),
    )]);
    let result = merge(
        &base,
        &ours,
        &theirs,
        Some(&MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configuration: &after,
            configurations: [&before, &after, &after],
            patches: &BTreeMap::new(),
            registry: None,
        }),
    )
    .unwrap();
    result.validate().unwrap();
    assert_eq!(
        result.project().unwrap()["snapshots"]["parent@1.0.0(shared@2.0.0)"]["dependencies"]["child"],
        "2.0.0(extra@2.0.0)(shared@2.0.0)"
    );
}
