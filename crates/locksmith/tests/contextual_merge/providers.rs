use super::{cfg, lock};
use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeTarget, PeerRules, WorkspaceConfig, inspect_peers,
    merge,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn a_peer_consumer_keeps_its_providers_repaired_dependency_context() {
    let make = |lib: u8, runtime: u8| {
        let provider = if runtime == 2 {
            "1.0.0(a@1.0.0)"
        } else {
            "1.0.0"
        };
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers":{".":{"dependencies":{
                "lib":{"specifier":format!("{lib}.0.0"),"version":format!("{lib}.0.0(runtime@{provider})")},
                "a":{"specifier":"1.0.0","version":"1.0.0"}
            }}},
            "packages":{
                format!("lib@{lib}.0.0"):{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"runtime":"*"}},
                "runtime@1.0.0":{"resolution":{"integrity":"sha512-runtime"}},
                "a@1.0.0":{"resolution":{"integrity":"sha512-a"}}
            },
            "snapshots":{
                format!("lib@{lib}.0.0(runtime@{provider})"):{"dependencies":{"runtime":provider}},
                format!("runtime@{provider}"):{},
                "a@1.0.0":{}
            }
        });
        if runtime == 2 {
            data["packageExtensionsChecksum"] = json!("checksum-final");
            data["packages"]["runtime@1.0.0"]["peerDependencies"] = json!({"a":"*"});
            data["snapshots"]["runtime@1.0.0(a@1.0.0)"] = json!({"dependencies":{"a":"1.0.0"}});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = WorkspaceConfig::default();
    let after = cfg(&json!({"packageExtensions":{"runtime@1.0.0":{"peerDependencies":{"a":"*"}}}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"lib":"2.0.0","a":"1.0.0"}}),
    )]);
    let result = merge(
        &make(1, 1),
        &make(1, 2),
        &make(2, 1),
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
    result.validate().unwrap();
    let data = result.project().unwrap();
    assert_eq!(
        data["importers"]["."]["dependencies"]["lib"]["version"],
        "2.0.0(runtime@1.0.0(a@1.0.0))"
    );
    assert_eq!(
        data["snapshots"]["lib@2.0.0(runtime@1.0.0(a@1.0.0))"]["dependencies"]["runtime"],
        "1.0.0(a@1.0.0)"
    );
}

#[test]
fn new_peer_requirements_preserve_other_peers_existing_policy() {
    let make = |wrapper: u8, upgraded: bool| {
        let runtime = if upgraded { 2 } else { 1 };
        let peers = if upgraded {
            "(a@1.0.0)(b@1.0.0(d@2.0.0))"
        } else {
            "(b@1.0.0(d@2.0.0))"
        };
        let runtime_peers = if upgraded { "(a@1.0.0)" } else { "" };
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "overrides":{"runtime":format!("{runtime}.0.0")},
            "importers":{".":{"dependencies":{
                "wrapper":{"specifier":format!("{wrapper}.0.0"),"version":format!("{wrapper}.0.0{peers}")},
                "a":{"specifier":"1.0.0","version":"1.0.0"},
                "b":{"specifier":"1.0.0","version":"1.0.0(d@1.0.0)"}
            }}},
            "packages":{
                format!("wrapper@{wrapper}.0.0"):{"resolution":{"integrity":"sha512-wrapper"},"peerDependencies":{"b":"*"}},
                format!("runtime@{runtime}.0.0"):{"resolution":{"integrity":"sha512-runtime"}},
                "a@1.0.0":{"resolution":{"integrity":"sha512-a"}},
                "b@1.0.0":{"resolution":{"integrity":"sha512-b"},"peerDependencies":{"d":"*"}},
                "d@1.0.0":{"resolution":{"integrity":"sha512-d1"}},
                "d@2.0.0":{"resolution":{"integrity":"sha512-d2"}}
            },
            "snapshots":{
                format!("wrapper@{wrapper}.0.0{peers}"):{"dependencies":{"runtime":format!("{runtime}.0.0{runtime_peers}"),"b":"1.0.0(d@2.0.0)"}},
                format!("runtime@{runtime}.0.0{runtime_peers}"):{},
                "a@1.0.0":{},
                "b@1.0.0(d@1.0.0)":{"dependencies":{"d":"1.0.0"}},
                "b@1.0.0(d@2.0.0)":{"dependencies":{"d":"2.0.0"}},
                "d@1.0.0":{},"d@2.0.0":{}
            }
        });
        if upgraded {
            data["packages"]["runtime@2.0.0"]["peerDependencies"] = json!({"a":"*"});
            data["snapshots"]["runtime@2.0.0(a@1.0.0)"] = json!({"dependencies":{"a":"1.0.0"}});
            data["snapshots"][format!("wrapper@{wrapper}.0.0{peers}")]["transitivePeerDependencies"] =
                json!(["a"]);
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = cfg(&json!({"overrides":{"runtime":"1.0.0"}}));
    let after = cfg(&json!({"overrides":{"runtime":"2.0.0"}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"wrapper":"2.0.0","a":"1.0.0","b":"1.0.0"}}),
    )]);
    let result = merge(
        &make(1, false),
        &make(1, true),
        &make(2, false),
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
    result.validate().unwrap();
    let data = result.project().unwrap();
    assert_eq!(
        data["importers"]["."]["dependencies"]["wrapper"]["version"],
        "2.0.0(a@1.0.0)(b@1.0.0(d@2.0.0))"
    );
    assert_eq!(
        data["snapshots"]["wrapper@2.0.0(a@1.0.0)(b@1.0.0(d@2.0.0))"]["dependencies"]["b"],
        "1.0.0(d@2.0.0)"
    );
    assert!(inspect_peers(&result, &PeerRules::default(), None).is_empty());
}

#[test]
fn new_peer_providers_preserve_or_rebind_their_nested_contexts() {
    let make = |wrapper: u8, upgraded: bool| {
        let runtime = if upgraded { 2 } else { 1 };
        let suffix = if upgraded { "(b@1.0.0)" } else { "" };
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"dedupePeers":true,"excludeLinksFromLockfile":false},
            "overrides":{"runtime":format!("{runtime}.0.0")},
            "importers":{".":{"dependencies":{
                "wrapper":{"specifier":format!("{wrapper}.0.0"),"version":format!("{wrapper}.0.0{suffix}")},
                "c":{"specifier":"1.0.0","version":"1.0.0(d@1.0.0)"},
                "d":{"specifier":"1.0.0","version":"1.0.0"}
            }}},
            "packages":{
                format!("wrapper@{wrapper}.0.0"):{"resolution":{"integrity":"sha512-wrapper"}},
                format!("runtime@{runtime}.0.0"):{"resolution":{"integrity":"sha512-runtime"}},
                "c@1.0.0":{"resolution":{"integrity":"sha512-c"},"peerDependencies":{"d":"*"}},
                "d@1.0.0":{"resolution":{"integrity":"sha512-d"}}
            },
            "snapshots":{
                format!("wrapper@{wrapper}.0.0{suffix}"):{"dependencies":{"runtime":format!("{runtime}.0.0{suffix}")}},
                format!("runtime@{runtime}.0.0{suffix}"):{},
                "c@1.0.0(d@1.0.0)":{"dependencies":{"d":"1.0.0"}},
                "d@1.0.0":{}
            }
        });
        data["packages"]["c@2.0.0"] = data["packages"]["c@1.0.0"].clone();
        data["snapshots"]["c@2.0.0(d@1.0.0)"] = data["snapshots"]["c@1.0.0(d@1.0.0)"].clone();
        if upgraded {
            data["snapshots"][format!("wrapper@{wrapper}.0.0{suffix}")]["transitivePeerDependencies"] =
                json!(["b"]);
            data["packages"]["runtime@2.0.0"]["peerDependencies"] = json!({"b":"*"});
            data["snapshots"]["runtime@2.0.0(b@1.0.0)"] =
                json!({"dependencies":{"b":"1.0.0(c@1.0.0)"}});
            data["packages"]["b@1.0.0"] = json!({"resolution":{"integrity":"sha512-b"}});
            data["packages"]["leaf@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-leaf"},"peerDependencies":{"c":"*"}});
            data["snapshots"]["b@1.0.0(c@1.0.0)"] = json!({"dependencies":{"leaf":"1.0.0(c@1.0.0)"},"transitivePeerDependencies":["c"]});
            data["snapshots"]["leaf@1.0.0(c@1.0.0)"] =
                json!({"dependencies":{"c":"1.0.0(d@1.0.0)"}});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = cfg(&json!({"overrides":{"runtime":"1.0.0"},"dedupePeers":true}));
    let after = cfg(&json!({"overrides":{"runtime":"2.0.0"},"dedupePeers":true}));
    for c in [1, 2] {
        let manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"wrapper":"2.0.0","c":format!("{c}.0.0"),"d":"1.0.0"}}),
        )]);
        let output = merge(
            &make(1, false),
            &make(1, true),
            &make(2, false),
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
        output.validate().unwrap();
        let data = output.project().unwrap();
        assert_eq!(
            data["snapshots"]["runtime@2.0.0(b@1.0.0)"]["dependencies"]["b"],
            format!("1.0.0(c@{c}.0.0)")
        );
        assert_eq!(
            data["snapshots"][format!("leaf@1.0.0(c@{c}.0.0)")]["dependencies"]["c"],
            format!("{c}.0.0(d@1.0.0)")
        );
    }
    let nested = |wrapper, upgraded| {
        let mut data = make(wrapper, upgraded).project().unwrap().clone();
        data["settings"]["dedupePeers"] = json!(false);
        Lockfile::parse(
            &data
                .to_string()
                .replace("(c@1.0.0)", "(c@1.0.0(d@1.0.0))")
                .replace("(b@1.0.0)", "(b@1.0.0(c@1.0.0(d@1.0.0)))"),
        )
        .unwrap()
    };
    let before = cfg(&json!({"overrides":{"runtime":"1.0.0"}}));
    let after = cfg(&json!({"overrides":{"runtime":"2.0.0"}}));
    let manifests = BTreeMap::from([
        (
            ".".into(),
            json!({"dependencies":{"wrapper":"2.0.0","c":"1.0.0","d":"1.0.0"}}),
        ),
        (
            "app".into(),
            json!({"dependencies":{"wrapper":"2.0.0","c":"2.0.0","d":"1.0.0"}}),
        ),
    ]);
    let output = merge(
        &nested(1, false),
        &nested(1, true),
        &nested(2, false),
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
    output.validate().unwrap();
    let data = output.project().unwrap();
    for (id, c) in [(".", 1), ("app", 2)] {
        let suffix = format!("(b@1.0.0(c@{c}.0.0(d@1.0.0)))");
        assert_eq!(
            data["importers"][id]["dependencies"]["wrapper"]["version"],
            format!("2.0.0{suffix}")
        );
        assert_eq!(
            data["snapshots"][format!("runtime@2.0.0{suffix}")]["dependencies"]["b"],
            format!("1.0.0(c@{c}.0.0(d@1.0.0))")
        );
    }
}

#[test]
fn shared_peer_instances_rebind_independently_across_workspace_roots() {
    let make = |app_a: u8, wrapper: bool| {
        let mut data = lock(app_a, 1, false).project().unwrap().clone();
        for a in 1..=3 {
            data["packages"][format!("a@{a}.0.0")] =
                json!({"resolution":{"integrity":format!("sha512-a{a}")}});
            data["snapshots"][format!("a@{a}.0.0")] = json!({});
        }
        data["importers"]["app"] = json!({"dependencies":{
            "a":{"specifier":"*","version":format!("{app_a}.0.0")},
            "b":{"specifier":"1.0.0","version":"1.0.0"}
        }});
        if wrapper {
            data["packages"]["wrapper@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-wrapper"}});
            data["snapshots"]["wrapper@1.0.0(a@2.0.0)(b@1.0.0)"] = json!({
                "dependencies":{"lib":"1.0.0(a@2.0.0)(b@1.0.0)"},
                "transitivePeerDependencies":["a","b"]
            });
            data["importers"]["app"]["dependencies"]["wrapper"] =
                json!({"specifier":"1.0.0","version":"1.0.0(a@2.0.0)(b@1.0.0)"});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, false);
    let ours = make(2, true);
    let theirs = make(1, false);
    let manifests = BTreeMap::from([
        (
            ".".into(),
            json!({"dependencies":{"a":"2.0.0","b":"1.0.0","lib":"1.0.0"}}),
        ),
        (
            "app".into(),
            json!({"dependencies":{"a":"3.0.0","b":"1.0.0","wrapper":"1.0.0"}}),
        ),
    ]);
    let config = WorkspaceConfig::default();
    let output = merge(
        &base,
        &ours,
        &theirs,
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
    let data = output.project().unwrap();
    assert_eq!(
        data["importers"]["."]["dependencies"]["lib"]["version"],
        "1.0.0(a@2.0.0)(b@1.0.0)"
    );
    assert_eq!(
        data["importers"]["app"]["dependencies"]["wrapper"]["version"],
        "1.0.0(a@3.0.0)(b@1.0.0)"
    );
    assert_eq!(
        data["snapshots"]["wrapper@1.0.0(a@3.0.0)(b@1.0.0)"]["dependencies"]["lib"],
        "1.0.0(a@3.0.0)(b@1.0.0)"
    );
}

#[test]
fn a_new_consumer_rebinds_borrowed_instances_to_its_workspace_root() {
    let mut data = lock(2, 1, false).project().unwrap().clone();
    data["importers"]["old"] = data["importers"]["."].clone();
    data["importers"]["."] = json!({"dependencies":{"a":{"specifier":"1.0.0","version":"1.0.0"},"b":{"specifier":"1.0.0","version":"1.0.0"}}});
    data["packages"]["a@1.0.0"] = json!({"resolution":{"integrity":"sha512-a"}});
    data["snapshots"]["a@1.0.0"] = json!({});
    let input = Lockfile::parse(&data.to_string()).unwrap();
    let manifests = BTreeMap::from([
        (
            ".".into(),
            json!({"dependencies":{"a":"1.0.0","b":"1.0.0"}}),
        ),
        ("new".into(), json!({"dependencies":{"lib":"1.0.0"}})),
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
        output.project().unwrap()["importers"]["new"]["dependencies"]["lib"]["version"],
        "1.0.0(a@1.0.0)(b@1.0.0)"
    );
}

#[test]
fn changing_one_provider_preserves_existing_policy_for_unchanged_providers() {
    let make = |b| {
        let mut data = lock(2, b, false).project().unwrap().clone();
        data["importers"]["."]["dependencies"]["a"] =
            json!({"specifier":"1.0.0","version":"1.0.0"});
        data["packages"]["a@1.0.0"] = json!({"resolution":{"integrity":"sha512-a"}});
        data["snapshots"]["a@1.0.0"] = json!({});
        data["packages"]["lib@1.0.0"]["peerDependencies"]["a"] = json!("^2");
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1);
    let changed = make(2);
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"1.0.0","b":"2.0.0","lib":"1.0.0"}}),
    )]);
    let config = json!({});
    let patches = BTreeMap::new();
    let result = merge(
        &base,
        &changed,
        &base,
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
        result.project().unwrap()["importers"]["."]["dependencies"]["lib"]["version"],
        "1.0.0(a@2.0.0)(b@2.0.0)"
    );
    assert!(inspect_peers(&result, &PeerRules::default(), None).is_empty());
}

#[test]
fn competing_new_peer_providers_use_the_consumers_declared_range() {
    for (range, expected, prerelease, child_range) in [
        ("2.0.0", Some("2.0.0"), false, None),
        ("1.0.0", Some("1.0.0"), false, None),
        ("*", None, false, None),
        ("3.0.0", None, false, None),
        ("1.0.0", None, true, None),
        (">=1.0.0-beta <2.0.0", Some("1.0.0-beta"), true, None),
        ("*", Some("2.0.0"), false, Some("2.0.0")),
    ] {
        let first = if prerelease { "1.0.0-beta" } else { "1.0.0" };
        let make = |wrapper: u8, upgraded: bool| {
            let runtime = if upgraded { 2 } else { 1 };
            let mut data = json!({
                "lockfileVersion":"9.0",
                "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
                "overrides":{"left":format!("{runtime}.0.0"),"right":format!("{runtime}.0.0")},
                "importers":{".":{"dependencies":{"wrapper":{"specifier":format!("{wrapper}.0.0"),"version":format!("{wrapper}.0.0")}}}},
                "packages":{
                    format!("wrapper@{wrapper}.0.0"): {"resolution":{"integrity":"sha512-wrapper"}},
                    format!("a@{first}"):{"resolution":{"integrity":"sha512-a1"}},
                    "a@2.0.0":{"resolution":{"integrity":"sha512-a2"}}
                },
                "snapshots":{
                    format!("wrapper@{wrapper}.0.0"): {"dependencies":{}},
                    format!("a@{first}"):{},"a@2.0.0":{}
                }
            });
            for (name, a) in [("left", first), ("right", "2.0.0")] {
                let suffix = if upgraded {
                    format!("(a@{a})")
                } else {
                    String::new()
                };
                data["packages"][format!("{name}@{runtime}.0.0")] =
                    json!({"resolution":{"integrity":format!("sha512-{name}")}});
                data["snapshots"][format!("{name}@{runtime}.0.0{suffix}")] = json!({});
                data["snapshots"][format!("wrapper@{wrapper}.0.0")]["dependencies"][name] =
                    json!(format!("{runtime}.0.0{suffix}"));
                if upgraded {
                    data["packages"][format!("{name}@2.0.0")]["peerDependencies"] =
                        json!({"a":range});
                    data["snapshots"][format!("{name}@2.0.0{suffix}")]["dependencies"] =
                        json!({"a":a});
                    if name == "left"
                        && let Some(range) = child_range
                    {
                        data["packages"]["child@1.0.0"] = json!({"resolution":{"integrity":"sha512-child"},"peerDependencies":{"a":range}});
                        data["snapshots"][format!("child@1.0.0{suffix}")] =
                            json!({"dependencies":{"a":a}});
                        data["snapshots"][format!("left@2.0.0{suffix}")]["dependencies"]["child"] =
                            json!(format!("1.0.0{suffix}"));
                    }
                }
            }
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let before = cfg(&json!({"overrides":{"left":"1.0.0","right":"1.0.0"}}));
        let after = cfg(&json!({"overrides":{"left":"2.0.0","right":"2.0.0"}}));
        for explicit in [false, true] {
            let mut manifests =
                BTreeMap::from([(".".into(), json!({"dependencies":{"wrapper":"2.0.0"}}))]);
            if explicit {
                manifests.get_mut(".").unwrap()["dependencies"]["a"] = json!(first);
            }
            let result = merge(
                &make(1, false),
                &make(1, true),
                &make(2, false),
                Some(&MergeContext {
                    target: MergeTarget::Theirs,
                    manifests: &manifests,
                    configurations: [&before, &after, &before],
                    configuration: &after,
                    patches: &BTreeMap::new(),
                    registry: None,
                }),
            );
            if let Some(a) = explicit.then_some(first).or(expected) {
                let result = result.unwrap();
                result.validate().unwrap();
                let data = result.project().unwrap();
                let wrapper = data["importers"]["."]["dependencies"]["wrapper"]["version"]
                    .as_str()
                    .unwrap();
                for name in ["left", "right"] {
                    assert_eq!(
                        data["snapshots"][format!("wrapper@{wrapper}")]["dependencies"][name],
                        format!("2.0.0(a@{a})")
                    );
                    assert_eq!(
                        data["snapshots"][format!("{name}@2.0.0(a@{a})")]["dependencies"]["a"],
                        a
                    );
                }
                if child_range.is_some() {
                    assert_eq!(
                        data["snapshots"][format!("child@1.0.0(a@{a})")]["dependencies"]["a"],
                        a
                    );
                }
            } else {
                assert!(
                    matches!(result, Err(LockfileError::ResolutionRequired { .. })),
                    "{range}: {result:?}"
                );
            }
        }
    }
}

#[test]
fn new_transitive_peer_releases_preserve_independent_descendant_contexts() {
    use sha2::{Digest, Sha256};
    let digest: String = Sha256::digest(b"b@1.0.0")[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let suffix = format!("({digest})");
    let make = |release: u8, upgraded: bool| {
        let runtime = if upgraded { "2.0.0" } else { "1.0.0" };
        let context = if upgraded { suffix.as_str() } else { "" };
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"dedupePeers":true,"excludeLinksFromLockfile":false},
            "overrides":{"runtime":runtime},
            "importers":{".":{"dependencies":{
                "wrapper":{"specifier":format!("{release}.0.0"),"version":format!("{release}.0.0{context}")},
                "c":{"specifier":"2.0.0","version":"2.0.0"}
            }}},
            "packages":{
                format!("wrapper@{release}.0.0"):{"resolution":{"integrity":"sha512-wrapper"}},
                format!("runtime@{runtime}"):{"resolution":{"integrity":"sha512-runtime"}},
                "c@2.0.0":{"resolution":{"integrity":"sha512-c2"}}
            },
            "snapshots":{
                format!("wrapper@{release}.0.0{context}"):{"dependencies":{"runtime":format!("{runtime}{context}")}},
                format!("runtime@{runtime}{context}"):{},
                "c@2.0.0":{}
            }
        });
        if upgraded {
            for name in ["wrapper", "runtime"] {
                let version = if name == "wrapper" { release } else { 2 };
                data["snapshots"][format!("{name}@{version}.0.0{context}")]["transitivePeerDependencies"] =
                    json!(["b"]);
            }
            data["packages"]["b@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-b"},"peerDependencies":{"c":"*"}});
            data["packages"]["c@1.0.0"] = json!({"resolution":{"integrity":"sha512-c1"}});
            data["snapshots"]["c@1.0.0"] = json!({});
            for (name, c) in [("left", 1), ("right", 2)] {
                data["packages"][format!("{name}@1.0.0")] = json!({"resolution":{"integrity":format!("sha512-{name}")},"peerDependencies":{"b":"*"}});
                data["snapshots"][format!("runtime@2.0.0{context}")]["dependencies"][name] =
                    json!("1.0.0(b@1.0.0)");
                data["snapshots"][format!("{name}@1.0.0(b@1.0.0)")] =
                    json!({"dependencies":{"b":format!("1.0.0(c@{c}.0.0)")}});
                data["snapshots"][format!("b@1.0.0(c@{c}.0.0)")] =
                    json!({"dependencies":{"c":format!("{c}.0.0")}});
            }
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, false);
    let ours = make(1, true);
    let theirs = make(2, false);
    let before = cfg(&json!({"overrides":{"runtime":"1.0.0"},"dedupePeers":true}));
    let after = cfg(&json!({"overrides":{"runtime":"2.0.0"},"dedupePeers":true}));
    for root_provider in [true, false] {
        let mut manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"wrapper":"2.0.0","c":"2.0.0"}}),
        )]);
        if !root_provider {
            manifests.get_mut(".").unwrap()["dependencies"]
                .as_object_mut()
                .unwrap()
                .remove("c");
        }
        let output = merge(
            &base,
            &ours,
            &theirs,
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
        output.validate().unwrap();
        let data = output.project().unwrap();
        assert_eq!(
            data["importers"]["."]["dependencies"]["wrapper"]["version"],
            "2.0.0(b@1.0.0)"
        );
        assert_eq!(
            data["snapshots"]["wrapper@2.0.0(b@1.0.0)"]["dependencies"]["runtime"],
            format!("2.0.0{suffix}")
        );
        for (name, c) in [("left", 1), ("right", 2)] {
            assert_eq!(
                data["snapshots"][format!("{name}@1.0.0(b@1.0.0)")]["dependencies"]["b"],
                format!("1.0.0(c@{c}.0.0)")
            );
        }
    }
}

#[test]
fn changed_peer_selectors_require_an_unmodified_declaration_witness() {
    let before = cfg(&json!({"overrides":{"a":"1.0.0"}}));
    let make = |populated: bool, overridden: bool| {
        let mut data = json!({"lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},"importers":{".":{}}});
        if overridden {
            data["overrides"] = json!({"a":"1.0.0"});
        }
        if populated {
            data["importers"]["."] =
                json!({"dependencies":{"lib":{"specifier":"1.0.0","version":"1.0.0(a@1.0.0)"}}});
            data["packages"] = json!({"lib@1.0.0":{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"a":"1.0.0"},"peerDependenciesMeta":{"a":{"optional":true}}},"a@1.0.0":{"resolution":{"integrity":"sha512-a"}}});
            data["snapshots"] =
                json!({"lib@1.0.0(a@1.0.0)":{"dependencies":{"a":"1.0.0"}},"a@1.0.0":{}});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    for overrides in [json!({}), json!({"a@1":"1.0.0"})] {
        let after = cfg(&json!({"overrides":overrides}));
        let mut target = make(false, false).project().unwrap().clone();
        target["overrides"] = overrides;
        let target = Lockfile::parse(&target.to_string()).unwrap();
        let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"lib":"1.0.0"}}))]);
        let result = merge(
            &make(false, true),
            &make(true, true),
            &target,
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&before, &before, &after],
                configuration: &after,
                patches: &BTreeMap::new(),
                registry: None,
            }),
        );
        let Err(LockfileError::ResolutionRequired { path, .. }) = result else {
            panic!("the original peer range is absent from every input")
        };
        assert_eq!(path, ["packages", "lib@1.0.0", "peerDependencies", "a"]);
    }
}

#[test]
fn added_local_providers_retire_only_obsolete_ancestor_peer_bindings() {
    for other_consumer in [false, true] {
        let before = WorkspaceConfig::default();
        let after = cfg(&json!({"packageExtensions":{"lib@1":{"dependencies":{"a":"2.0.0"}}}}));
        let mut data = json!({
            "lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers":{".":{"dependencies":{"wrapper":{"specifier":"1.0.0","version":"1.0.0(a@1.0.0)"},"a":{"specifier":"1.0.0","version":"1.0.0"}}}},
            "packages":{"wrapper@1.0.0":{"resolution":{"integrity":"sha512-wrapper"}},"lib@1.0.0":{"resolution":{"integrity":"sha512-lib"}},"consumer@1.0.0":{"resolution":{"integrity":"sha512-consumer"},"peerDependencies":{"a":"*"}},"a@1.0.0":{"resolution":{"integrity":"sha512-a1"}}},
            "snapshots":{"wrapper@1.0.0(a@1.0.0)":{"dependencies":{"lib":"1.0.0(a@1.0.0)"},"transitivePeerDependencies":["a"]},"lib@1.0.0(a@1.0.0)":{"dependencies":{"consumer":"1.0.0(a@1.0.0)"},"transitivePeerDependencies":["a"]},"consumer@1.0.0(a@1.0.0)":{"dependencies":{"a":"1.0.0"}},"a@1.0.0":{}}
        });
        if other_consumer {
            data["snapshots"]["wrapper@1.0.0(a@1.0.0)"]["dependencies"]["consumer"] =
                json!("1.0.0(a@1.0.0)");
        }
        let source = Lockfile::parse(&data.to_string()).unwrap();
        let donor=Lockfile::parse(&json!({
            "lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},"packageExtensionsChecksum":"sha256-extended",
            "importers":{".":{"dependencies":{"a":{"specifier":"2.0.0","version":"2.0.0"}}}},
            "packages":{"a@2.0.0":{"resolution":{"integrity":"sha512-a2"}}},"snapshots":{"a@2.0.0":{}}
        }).to_string()).unwrap();
        let manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"wrapper":"1.0.0","a":"1.0.0"}}),
        )]);
        let output = merge(
            &source,
            &source,
            &donor,
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
        let doc = output.project().unwrap();
        let wrapper = if other_consumer {
            "1.0.0(a@1.0.0)"
        } else {
            "1.0.0"
        };
        assert_eq!(
            doc["importers"]["."]["dependencies"]["wrapper"]["version"],
            wrapper
        );
        assert_eq!(
            doc["snapshots"][format!("wrapper@{wrapper}")]["dependencies"]["lib"],
            "1.0.0"
        );
        assert_eq!(
            doc["snapshots"]["lib@1.0.0"]["dependencies"],
            json!({"a":"2.0.0","consumer":"1.0.0(a@2.0.0)"})
        );
        assert!(
            doc["snapshots"]["lib@1.0.0"]
                .get("transitivePeerDependencies")
                .is_none()
        );
        if other_consumer {
            assert_eq!(
                doc["snapshots"][format!("wrapper@{wrapper}")]["dependencies"]["consumer"],
                "1.0.0(a@1.0.0)"
            );
        }
        output.validate().unwrap();
    }
}
