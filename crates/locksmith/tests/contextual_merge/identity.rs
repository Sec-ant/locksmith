use super::{cfg, lock};
use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeTarget, Value, WorkspaceConfig, merge,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn converging_peer_contexts_prefer_the_existing_instance_and_keep_required_usage() {
    for reversed in [false, true] {
        let make = |release: u8, upgraded: bool| {
            let runtime = if upgraded { 2 } else { 1 };
            let first = if upgraded { 2 } else { 1 };
            let second = 2;
            let mut data = json!({
                "lockfileVersion":"9.0",
                "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
                "overrides":{"runtime":format!("{runtime}.0.0")},
                "importers":{".":{"dependencies":{"wrapper":{"specifier":format!("{release}.0.0"),"version":format!("{release}.0.0")}}}},
                "packages":{
                    format!("wrapper@{release}.0.0"):{"resolution":{"integrity":"sha512-wrapper"}},
                    format!("trigger@{release}.0.0"):{"resolution":{"integrity":"sha512-trigger"}},
                    format!("lib@{release}.0.0"):{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"a":"*"}},
                    format!("runtime@{runtime}.0.0"):{"resolution":{"integrity":"sha512-runtime"}},
                    "a@1.0.0":{"resolution":{"integrity":"sha512-a1"}},
                    "a@2.0.0":{"resolution":{"integrity":"sha512-a2"}},
                    "util@1.0.0":{"resolution":{"integrity":"sha512-util1"}},
                    "util@2.0.0":{"resolution":{"integrity":"sha512-util2"}}
                },
                "snapshots":{
                    format!("wrapper@{release}.0.0"):{"dependencies":{
                        "a":"2.0.0",
                        "trigger":if upgraded { format!("{release}.0.0(a@2.0.0)") } else { format!("{release}.0.0") },
                        "left":format!("lib@{release}.0.0(a@{first}.0.0)"),
                        "right":format!("lib@{release}.0.0(a@{second}.0.0)")
                    }},
                    "a@1.0.0":{},"a@2.0.0":{},"util@1.0.0":{},"util@2.0.0":{}
                }
            });
            let optional_alias = if reversed { "right" } else { "left" };
            let wrapper = &mut data["snapshots"][format!("wrapper@{release}.0.0")];
            let optional = wrapper["dependencies"]
                .as_object_mut()
                .unwrap()
                .remove(optional_alias)
                .unwrap();
            wrapper["optionalDependencies"] = json!({optional_alias:optional});
            if upgraded {
                data["packages"]["runtime@2.0.0"]["peerDependencies"] = json!({"a":"*"});
                data["snapshots"][format!("trigger@{release}.0.0(a@2.0.0)")] = json!({"dependencies":{"runtime":"2.0.0(a@2.0.0)"},"transitivePeerDependencies":["a"]});
            } else {
                data["snapshots"][format!("trigger@{release}.0.0")] =
                    json!({"dependencies":{"runtime":"1.0.0"}});
            }
            for a in [1, 2] {
                let runtime_ref = if upgraded {
                    format!("2.0.0(a@{a}.0.0)")
                } else {
                    "1.0.0".into()
                };
                let key = format!("lib@{release}.0.0(a@{a}.0.0)");
                data["snapshots"][&key] = json!({"dependencies":{"a":format!("{a}.0.0"),"runtime":runtime_ref,"util":format!("{a}.0.0")}});
                if !upgraded && ((a == 2) == reversed) {
                    data["snapshots"][&key]["optional"] = json!(true);
                }
                data["snapshots"][format!("runtime@{runtime_ref}")] = if upgraded {
                    json!({"dependencies":{"a":format!("{a}.0.0")}})
                } else {
                    json!({})
                };
            }
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let before = cfg(&json!({"overrides":{"runtime":"1.0.0"}}));
        let after = cfg(&json!({"overrides":{"runtime":"2.0.0"}}));
        let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"wrapper":"2.0.0"}}))]);
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
        for alias in ["left", "right"] {
            assert_eq!(
                data["snapshots"]["wrapper@2.0.0"]["dependencies"]
                    .get(alias)
                    .or_else(
                        || data["snapshots"]["wrapper@2.0.0"]["optionalDependencies"].get(alias)
                    )
                    .unwrap(),
                "lib@2.0.0(a@2.0.0)"
            );
        }
        let instance = &data["snapshots"]["lib@2.0.0(a@2.0.0)"];
        assert_eq!(instance["dependencies"]["util"], "2.0.0");
        assert_eq!(instance["dependencies"]["runtime"], "2.0.0(a@2.0.0)");
        assert!(instance.get("optional").is_none());
    }
}

#[test]
fn compatible_source_instances_replace_generated_instances_from_older_configuration() {
    for (donor_provider, shared_parent) in [(2, false), (3, false), (2, true)] {
        let make = |wrapper: u8, provider: u8, extended: bool| {
            let mut data = lock(provider, 1, false).project().unwrap().clone();
            let suffix = format!("(a@{provider}.0.0)(b@1.0.0)");
            data["importers"]["."]["dependencies"]
                .as_object_mut()
                .unwrap()
                .remove("lib");
            data["importers"]["."]["dependencies"]["wrapper"] = json!({"specifier":format!("{wrapper}.0.0"),"version":format!("{wrapper}.0.0{suffix}")});
            data["packages"][format!("wrapper@{wrapper}.0.0")] =
                json!({"resolution":{"integrity":"sha512-wrapper"}});
            data["snapshots"][format!("wrapper@{wrapper}.0.0{suffix}")] = json!({"dependencies":{"lib":format!("1.0.0{suffix}")},"transitivePeerDependencies":["a","b"]});
            data["packages"]["a@2.0.0"] = json!({"resolution":{"integrity":"sha512-a"}});
            data["snapshots"]["a@2.0.0"] = json!({});
            if shared_parent {
                data["importers"]["."]["dependencies"]["z-stable"] =
                    json!({"specifier":"1.0.0","version":format!("1.0.0{suffix}")});
                data["packages"]["z-stable@1.0.0"] =
                    json!({"resolution":{"integrity":"sha512-stable"}});
                data["snapshots"][format!("z-stable@1.0.0{suffix}")] = json!({"dependencies":{"lib":format!("1.0.0{suffix}")},"transitivePeerDependencies":["a","b"]});
            }
            if extended {
                data["packageExtensionsChecksum"] = json!("checksum-final");
                data["packages"]["extra@1.0.0"] =
                    json!({"resolution":{"integrity":"sha512-extra"}});
                data["snapshots"]["extra@1.0.0"] = json!({});
                data["snapshots"][format!("lib@1.0.0{suffix}")]["dependencies"]["extra"] =
                    json!("1.0.0");
            }
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let before = WorkspaceConfig::default();
        let after =
            cfg(&json!({"packageExtensions":{"lib@1.0.0":{"dependencies":{"extra":"1.0.0"}}}}));
        let mut manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"a":"2.0.0","b":"1.0.0","wrapper":"2.0.0"}}),
        )]);
        if shared_parent {
            manifests.get_mut(".").unwrap()["dependencies"]["z-stable"] = json!("1.0.0");
        }
        let result = merge(
            &make(1, 1, false),
            &make(1, donor_provider, true),
            &make(2, 1, false),
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
            "2.0.0(a@2.0.0)(b@1.0.0)"
        );
        assert_eq!(
            data["snapshots"]["wrapper@2.0.0(a@2.0.0)(b@1.0.0)"]["dependencies"]["lib"],
            "1.0.0(a@2.0.0)(b@1.0.0)"
        );
        assert_eq!(
            data["snapshots"]["lib@1.0.0(a@2.0.0)(b@1.0.0)"]["dependencies"],
            json!({"a":"2.0.0","b":"1.0.0","extra":"1.0.0"})
        );
    }
}

#[test]
fn workspace_context_recovers_hashes_with_shadowed_transitive_peers() {
    let make = |a: u8, b: u8, hash: &str| {
        let mut data = lock(a, b, false).project().unwrap().clone();
        data["packages"]["wrapper@1.0.0"] = json!({"resolution":{"integrity":"sha512-wrapper"}});
        data["packages"]["local@1.0.0"] =
            json!({"resolution":{"integrity":"sha512-local"},"peerDependencies":{"a":"*","b":"*"}});
        data["packages"]["a@3.0.0"] = json!({"resolution":{"integrity":"sha512-a3"}});
        data["snapshots"]["a@3.0.0"] = json!({});
        data["snapshots"][format!("local@1.0.0(a@3.0.0)(b@{b}.0.0)")] =
            json!({"dependencies":{"a":"3.0.0","b":format!("{b}.0.0")}});
        data["snapshots"][format!("wrapper@1.0.0({hash})")] = json!({"dependencies":{"lib":format!("1.0.0(a@{a}.0.0)(b@{b}.0.0)"),"local":format!("1.0.0(a@3.0.0)(b@{b}.0.0)")},"transitivePeerDependencies":["a","b"]});
        data["importers"]["."]["dependencies"]
            .as_object_mut()
            .unwrap()
            .remove("lib");
        data["importers"]["."]["dependencies"]["wrapper"] =
            json!({"specifier":"1.0.0","version":format!("1.0.0({hash})")});
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, 1, "bff246112e0e73444993332af97741a4");
    let ours = make(2, 1, "9ce283e4780c68eed3dd89aeb9cb67a4");
    let theirs = make(1, 2, "1b449cb0abeb74747f6b778d225da4b0");
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"2.0.0","b":"2.0.0","wrapper":"1.0.0"}}),
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
    let output = merge(&base, &ours, &theirs, Some(&context)).unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["."]["dependencies"]["wrapper"]["version"],
        "1.0.0(06ce7e7c3b17c0697fd17d45286369ad)"
    );
    assert_eq!(
        output.project().unwrap()["snapshots"]["local@1.0.0(a@3.0.0)(b@2.0.0)"]["dependencies"]["a"],
        "3.0.0"
    );
}

#[test]
fn hashed_parents_recover_peers_bound_only_in_deduplicated_descendants() {
    use sha2::{Digest, Sha256};
    let fixed: Vec<_> = (0..3)
        .map(|i| format!("@scope/{i}-{}", "provider-".repeat(22)))
        .collect();
    let prefix: String = fixed.iter().map(|name| format!("{name}@1.0.0)(")).collect();
    for optional in [true, false] {
        let identity = |a, b| {
            let hash: String = Sha256::digest(format!("{prefix}a@{a}.0.0)(b@{b}.0.0"))[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("1.0.0({hash})")
        };
        let make = |a, b| {
            let mut data = lock(a, b, false).project().unwrap().clone();
            data["packages"]["wrapper@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-wrapper"}});
            data["packages"]["optional@1.0.0"] = json!({
                "resolution":{"integrity":"sha512-optional"},"peerDependencies":{"x":"*","y":"*"},
                "peerDependenciesMeta":{"x":{"optional":optional},"y":{"optional":optional}}
            });
            for name in ["x", "y"] {
                data["packages"][format!("{name}@1.0.0")] =
                    json!({"resolution":{"integrity":format!("sha512-{name}")}});
                data["snapshots"][format!("{name}@1.0.0")] = json!({});
                data["importers"]["."]["dependencies"][name] =
                    json!({"specifier":"1.0.0","version":"1.0.0"});
            }
            data["snapshots"]["optional@1.0.0(x@1.0.0)(y@1.0.0)"] =
                json!({"dependencies":{"x":"1.0.0","y":"1.0.0"}});
            data["snapshots"][format!("wrapper@{}", identity(a, b))] = json!({"dependencies":{
            "lib":format!("1.0.0(a@{a}.0.0)(b@{b}.0.0)"),"optional":"1.0.0(x@1.0.0)(y@1.0.0)"
        },"transitivePeerDependencies":["a","b","x","y"]});
            for name in &fixed {
                data["packages"][format!("{name}@1.0.0")] =
                    json!({"resolution":{"integrity":"sha512-fixed"}});
                data["snapshots"][format!("{name}@1.0.0")] = json!({});
                data["packages"]["wrapper@1.0.0"]["peerDependencies"][name] = json!("*");
                data["snapshots"][format!("wrapper@{}", identity(a, b))]["dependencies"][name] =
                    json!("1.0.0");
                data["importers"]["."]["dependencies"][name] =
                    json!({"specifier":"1.0.0","version":"1.0.0"});
            }
            data["importers"]["."]["dependencies"]
                .as_object_mut()
                .unwrap()
                .remove("lib");
            data["importers"]["."]["dependencies"]["wrapper"] =
                json!({"specifier":"1.0.0","version":identity(a,b)});
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let mut root = json!({"dependencies":{"a":"2.0.0","b":"2.0.0","wrapper":"1.0.0","x":"1.0.0","y":"1.0.0"}});
        for name in &fixed {
            root["dependencies"][name] = json!("1.0.0");
        }
        let manifests = BTreeMap::from([(".".into(), root)]);
        let config = cfg(&json!({}));
        let patches = BTreeMap::new();
        let result = merge(
            &make(1, 1),
            &make(2, 1),
            &make(1, 2),
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &patches,
                registry: None,
            }),
        )
        .unwrap();
        result.validate().unwrap();
        let data = result.project().unwrap();
        assert_eq!(
            data["importers"]["."]["dependencies"]["wrapper"]["version"],
            identity(2, 2)
        );
        assert_eq!(
            data["snapshots"][format!("wrapper@{}", identity(2, 2))]["dependencies"]["optional"],
            "1.0.0(x@1.0.0)(y@1.0.0)"
        );
    }
}

#[test]
fn aliased_peer_providers_update_both_identity_and_dependency_edge() {
    let make = |a: u8, b: u8| {
        let raw = lock(a, b, false)
            .project()
            .unwrap()
            .to_string()
            .replace("a@", "real-a@");
        let mut data: Value = serde_json::from_str(&raw).unwrap();
        data["importers"]["."]["dependencies"]["a"] =
            json!({"specifier":"npm:real-a@*","version":format!("real-a@{a}.0.0")});
        data["snapshots"][format!("lib@1.0.0(real-a@{a}.0.0)(b@{b}.0.0)")]["dependencies"]["a"] =
            json!(format!("real-a@{a}.0.0"));
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, 1);
    let ours = make(2, 1);
    let theirs = make(1, 2);
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"npm:real-a@2.0.0","b":"2.0.0","lib":"1.0.0"}}),
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
    let output = merge(&base, &ours, &theirs, Some(&context)).unwrap();
    let doc = output.project().unwrap();
    let reference = doc["importers"]["."]["dependencies"]["lib"]["version"]
        .as_str()
        .unwrap();
    assert_eq!(
        doc["snapshots"][format!("lib@{reference}")]["dependencies"]["a"],
        "real-a@2.0.0"
    );
}

#[test]
fn distinct_aliases_of_one_peer_package_keep_independent_bindings() {
    use sha2::{Digest, Sha256};
    for hashed in [false, true] {
        for wrapped in [false, true] {
            let suffix = |real, alias| {
                let mut ids = [format!("real@{real}.0.0"), format!("real@{alias}.0.0")];
                ids.sort();
                let text = ids.join(")(");
                let text = if hashed {
                    Sha256::digest(text.as_bytes())[..16]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect()
                } else {
                    text
                };
                format!("({text})")
            };
            let make = |real, alias| {
                let peer_suffix = suffix(real, alias);
                let mut data = json!({"lockfileVersion":"9.0", "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
                    "importers":{".":{"dependencies":{
                        "real":{"specifier":"*","version":format!("{real}.0.0")},
                        "alias":{"specifier":"npm:real@*","version":format!("real@{alias}.0.0")},
                        "lib":{"specifier":"1.0.0","version":format!("1.0.0{peer_suffix}")}
                    }}},
                    "packages":{"lib@1.0.0":{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"real":"*","alias":"*"}}},
                    "snapshots":{format!("lib@1.0.0{peer_suffix}"):{"dependencies":{"real":format!("{real}.0.0"),"alias":format!("real@{alias}.0.0")}}}
                });
                for version in [real, alias] {
                    data["packages"][format!("real@{version}.0.0")] =
                        json!({"resolution":{"integrity":format!("sha512-real-{version}")}});
                    data["snapshots"][format!("real@{version}.0.0")] = json!({});
                }
                if wrapped {
                    data["packages"]["wrapper@1.0.0"] =
                        json!({"resolution":{"integrity":"sha512-wrapper"}});
                    data["snapshots"][format!("wrapper@1.0.0{peer_suffix}")] = json!({"dependencies":{"lib":format!("1.0.0{peer_suffix}")},"transitivePeerDependencies":["alias","real"]});
                    let dependency = data["importers"]["."]["dependencies"]
                        .as_object_mut()
                        .unwrap()
                        .remove("lib")
                        .unwrap();
                    data["importers"]["."]["dependencies"]["wrapper"] = dependency;
                }
                Lockfile::parse(&data.to_string()).unwrap()
            };
            let root = if wrapped { "wrapper" } else { "lib" };
            let manifests = BTreeMap::from([(
                ".".into(),
                json!({"dependencies":{"real":"4.0.0","alias":"npm:real@5.0.0",root:"1.0.0"}}),
            )]);
            let config = cfg(&json!({}));
            let patches = BTreeMap::new();
            // Both providers initially resolve the same release. The branches
            // then update the real name and its alias independently.
            let output = merge(
                &make(1, 1),
                &make(4, 1),
                &make(1, 5),
                Some(&MergeContext {
                    target: MergeTarget::Theirs,
                    manifests: &manifests,
                    configurations: [&config; 3],
                    configuration: &config,
                    patches: &patches,
                    registry: None,
                }),
            )
            .unwrap();
            output.validate().unwrap();
            let data = output.project().unwrap();
            assert_eq!(
                data["importers"]["."]["dependencies"][root]["version"],
                format!("1.0.0{}", suffix(4, 5))
            );
            let body = &data["snapshots"][format!("lib@1.0.0{}", suffix(4, 5))];
            assert_eq!(body["dependencies"]["real"], "4.0.0");
            assert_eq!(body["dependencies"]["alias"], "real@5.0.0");
        }
    }
}

#[test]
fn a_local_provider_replaces_an_inherited_alias_by_its_declared_peer_name() {
    let input = Lockfile::parse(&json!({
        "lockfileVersion":"9.0",
        "importers":{
            ".":{"dependencies":{"compiler":{"specifier":"npm:compiler-next@1.0.0","version":"compiler-next@1.0.0"}}},
            "app":{"dependencies":{"tool":{"specifier":"1.0.0","version":"1.0.0(compiler-next@1.0.0)"}}},
            "seed":{"dependencies":{"compiler":{"specifier":"3.0.0","version":"3.0.0"}}}
        },
        "packages":{
            "compiler-next@1.0.0":{"resolution":{"integrity":"sha512-next"}},
            "compiler@3.0.0":{"resolution":{"integrity":"sha512-compiler"}},
            "tool@1.0.0":{"resolution":{"integrity":"sha512-tool"},"peerDependencies":{"compiler":"*"}}
        },
        "snapshots":{
            "compiler-next@1.0.0":{},"compiler@3.0.0":{},
            "tool@1.0.0(compiler-next@1.0.0)":{"dependencies":{"compiler":"compiler-next@1.0.0"}}
        }
    }).to_string()).unwrap();
    let manifests = BTreeMap::from([
        (
            ".".into(),
            json!({"dependencies":{"compiler":"npm:compiler-next@1.0.0"}}),
        ),
        (
            "app".into(),
            json!({"dependencies":{"compiler":"3.0.0","tool":"1.0.0"}}),
        ),
        ("seed".into(), json!({"dependencies":{"compiler":"3.0.0"}})),
    ]);
    let config = cfg(&json!({}));
    let output = merge(
        &input,
        &input,
        &input,
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
        data["importers"]["."]["dependencies"]["compiler"]["version"],
        "compiler-next@1.0.0"
    );
    assert_eq!(
        data["importers"]["app"]["dependencies"]["tool"]["version"],
        "1.0.0(compiler@3.0.0)"
    );
    assert_eq!(
        data["snapshots"]["tool@1.0.0(compiler@3.0.0)"]["dependencies"]["compiler"],
        "3.0.0"
    );
}

#[test]
fn consumers_converging_to_one_peer_identity_use_target_regular_dependencies() {
    let make = |a, b, runtime, id: &str| {
        let mut data = lock(a, b, false).project().unwrap().clone();
        data["importers"][id] = data["importers"]["."].clone();
        data["importers"]["."] = json!({});
        data["packages"][format!("runtime@{runtime}.0.0")] =
            json!({"resolution":{"integrity":"sha512-runtime"}});
        data["snapshots"][format!("runtime@{runtime}.0.0")] = json!({});
        data["snapshots"][format!("lib@1.0.0(a@{a}.0.0)(b@{b}.0.0)")]["dependencies"]["runtime"] =
            json!(format!("{runtime}.0.0"));
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, 1, 1, "first");
    let ours = make(2, 2, 2, "first");
    let theirs = make(3, 1, 3, "second");
    let manifest = json!({"dependencies":{"a":"3.0.0","b":"2.0.0","lib":"1.0.0"}});
    let manifests = BTreeMap::from([
        (".".into(), json!({})),
        ("first".into(), manifest.clone()),
        ("second".into(), manifest),
    ]);
    let config = json!({});
    let patches = BTreeMap::new();
    let output = merge(
        &base,
        &ours,
        &theirs,
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
        output.project().unwrap()["snapshots"]["lib@1.0.0(a@3.0.0)(b@2.0.0)"]["dependencies"]["runtime"],
        "3.0.0"
    );
    output.validate().unwrap();
}

#[test]
fn flat_transitive_hashes_preserve_distinct_nested_provider_contexts() {
    use sha2::{Digest, Sha256};
    let make = |a: u8, b: u8| {
        let mut data = lock(a, b, false).project().unwrap().clone();
        let text = format!("a@{a}.0.0)(b@{b}.0.0)(provider@1.0.0");
        let hash: String = Sha256::digest(text.as_bytes())[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let wrapper = format!("wrapper@1.0.0({hash})");
        data["packages"]["wrapper@1.0.0"] = json!({"resolution":{"integrity":"sha512-wrapper"}});
        data["snapshots"][&wrapper] = json!({"dependencies":{"lib":format!("1.0.0(a@{a}.0.0)(b@{b}.0.0)")},"transitivePeerDependencies":["a","b","provider"]});
        data["importers"]["."]["dependencies"]
            .as_object_mut()
            .unwrap()
            .remove("lib");
        data["importers"]["."]["dependencies"]["wrapper"] =
            json!({"specifier":"1.0.0","version":format!("1.0.0({hash})")});
        data["packages"]["provider@1.0.0"] =
            json!({"resolution":{"integrity":"sha512-provider"},"peerDependencies":{"x":"*"}});
        data["packages"]["lib@1.0.0"]["peerDependencies"]["provider"] = json!("*");
        let old_lib = format!("lib@1.0.0(a@{a}.0.0)(b@{b}.0.0)");
        let lib_ref = format!("1.0.0(a@{a}.0.0)(b@{b}.0.0)(provider@1.0.0(x@1.0.0))");
        let mut lib = data["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove(&old_lib)
            .unwrap();
        lib["dependencies"]["provider"] = json!("1.0.0(x@1.0.0)");
        data["snapshots"][format!("lib@{lib_ref}")] = lib;
        data["snapshots"][&wrapper]["dependencies"]["lib"] = json!(lib_ref);
        for (name, x) in [("left", 1), ("right", 2)] {
            data["packages"][format!("x@{x}.0.0")] = json!({"resolution":{"integrity":"sha512-x"}});
            data["snapshots"][format!("x@{x}.0.0")] = json!({});
            data["snapshots"][format!("provider@1.0.0(x@{x}.0.0)")] =
                json!({"dependencies":{"x":format!("{x}.0.0")}});
            data["packages"][format!("{name}@1.0.0")] = json!({"resolution":{"integrity":"sha512-child"},"peerDependencies":{"provider":"*"}});
            let reference = format!("1.0.0(provider@1.0.0(x@{x}.0.0))");
            data["snapshots"][format!("{name}@{reference}")] =
                json!({"dependencies":{"provider":format!("1.0.0(x@{x}.0.0)")}});
            data["snapshots"][&wrapper]["dependencies"][name] = json!(reference);
        }
        // A second version appears in a descendant's scope; only the source
        // hash identifies which version belongs to the wrapper's peer context.
        data["packages"]["provider@2.0.0"] = json!({"resolution":{"integrity":"sha512-provider2"}});
        data["snapshots"]["provider@2.0.0"] = json!({});
        data["packages"]["shadow@1.0.0"] =
            json!({"resolution":{"integrity":"sha512-shadow"},"peerDependencies":{"provider":"*"}});
        data["snapshots"]["shadow@1.0.0(provider@2.0.0)"] =
            json!({"dependencies":{"provider":"2.0.0"}});
        data["snapshots"][&wrapper]["dependencies"]["shadow"] = json!("1.0.0(provider@2.0.0)");
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"2.0.0","b":"2.0.0","wrapper":"1.0.0"}}),
    )]);
    let config = cfg(&json!({}));
    let patches = BTreeMap::new();
    let context = MergeContext {
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&config; 3],
        configuration: &config,
        patches: &patches,
        registry: None,
    };
    let output = merge(&make(1, 1), &make(2, 1), &make(1, 2), Some(&context)).unwrap();
    output.validate().unwrap();
    let mut expected = make(2, 2).project().unwrap().clone();
    for name in ["a", "b"] {
        expected["importers"]["."]["dependencies"][name]["specifier"] =
            manifests["."]["dependencies"][name].clone();
    }
    assert_eq!(output.project(), Some(&expected));
}

#[test]
fn explicit_peer_deduplication_coalesces_nested_contexts() {
    let importer = json!({"dependencies":{
        "a":{"specifier":"1.0.0","version":"1.0.0"},
        "b":{"specifier":"1.0.0","version":"1.0.0(a@1.0.0)"},
        "c":{"specifier":"1.0.0","version":"1.0.0"},
        "lib":{"specifier":"1.0.0","version":"1.0.0(b@1.0.0)(c@1.0.0)"}
    }});
    let input = Lockfile::parse(
        &json!({
            "lockfileVersion":"9.0",
            "importers":{".":importer,"app":importer},
            "packages":{
                "a@1.0.0":{"resolution":{"integrity":"sha512-a1"}},
                "a@2.0.0":{"resolution":{"integrity":"sha512-a2"}},
                "c@1.0.0":{"resolution":{"integrity":"sha512-c1"}},
                "c@2.0.0":{"resolution":{"integrity":"sha512-c2"}},
                "b@1.0.0":{"resolution":{"integrity":"sha512-b"},"peerDependencies":{"a":"*"}},
                "lib@1.0.0":{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"b":"*","c":"*"}}
            },
            "snapshots":{
                "a@1.0.0":{},"a@2.0.0":{},
                "c@1.0.0":{},"c@2.0.0":{},
                "b@1.0.0(a@1.0.0)":{"dependencies":{"a":"1.0.0"}},
                "lib@1.0.0(b@1.0.0)(c@1.0.0)":{"dependencies":{"b":"1.0.0(a@1.0.0)","c":"1.0.0"}}
            }
        })
        .to_string(),
    )
    .unwrap();
    let manifests = BTreeMap::from([
        (
            ".".into(),
            json!({"dependencies":{"a":"1.0.0","b":"1.0.0","c":"2.0.0","lib":"1.0.0"}}),
        ),
        (
            "app".into(),
            json!({"dependencies":{"a":"2.0.0","b":"1.0.0","c":"2.0.0","lib":"1.0.0"}}),
        ),
    ]);
    let patches = BTreeMap::new();
    let resolve = |dedupe| {
        let config = cfg(&json!({"dedupePeers":dedupe}));
        merge(
            &input,
            &input,
            &input,
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &patches,
                registry: None,
            }),
        )
    };
    assert!(matches!(
        resolve(false),
        Err(LockfileError::ResolutionRequired { .. })
    ));
    let output = resolve(true).unwrap();
    output.validate().unwrap();
    let result = output.project().unwrap();
    assert_eq!(
        result["importers"]["."]["dependencies"]["lib"]["version"],
        "1.0.0(b@1.0.0)(c@2.0.0)"
    );
    assert_eq!(
        result["snapshots"]["lib@1.0.0(b@1.0.0)(c@2.0.0)"]["dependencies"]["b"],
        "1.0.0(a@1.0.0)"
    );
    assert_eq!(
        result["importers"]["app"]["dependencies"]["lib"]["version"],
        "1.0.0(b@1.0.0)(c@2.0.0)"
    );
    assert_eq!(
        result["importers"]["app"]["dependencies"]["b"]["version"],
        "1.0.0(a@2.0.0)"
    );
}

#[test]
fn an_existing_opaque_instance_is_reused_in_its_unchanged_provider_context() {
    // Treat an unverified hash as an opaque identity supplied by a lockfile.
    let suffix = "ffffffffffffffffffffffffffffffff";
    let make = |provider: u8| {
        let mut data = lock(1, 1, false).project().unwrap().clone();
        let body = data["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove("lib@1.0.0(a@1.0.0)(b@1.0.0)")
            .unwrap();
        data["snapshots"][format!("lib@1.0.0({suffix})")] = body;
        data["importers"]["."]["dependencies"]["lib"]["version"] =
            json!(format!("1.0.0({suffix})"));
        data["importers"]["."]["dependencies"]["a"]["version"] = json!(format!("{provider}.0.0"));
        data["packages"][format!("a@{provider}.0.0")] =
            json!({"resolution":{"integrity":"sha512-a"}});
        data["snapshots"][format!("a@{provider}.0.0")] = json!({});
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1);
    let ours = make(2);
    let config = WorkspaceConfig::default();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"2.0.0","b":"1.0.0","lib":"1.0.0"}}),
    )]);
    let patches = BTreeMap::new();
    let result = merge(
        &base,
        &ours,
        &base,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &patches,
            registry: None,
        }),
    )
    .unwrap();
    result.validate().unwrap();
    assert_eq!(
        result.project().unwrap()["importers"]["."]["dependencies"]["lib"]["version"],
        format!("1.0.0({suffix})")
    );
    assert_eq!(
        result.project().unwrap()["snapshots"][format!("lib@1.0.0({suffix})")]["dependencies"]["a"],
        "1.0.0"
    );
}

#[test]
fn opaque_context_candidates_are_selected_separately_for_each_workspace() {
    use sha2::{Digest, Sha256};
    let opaque = "1.0.0(ffffffffffffffffffffffffffffffff)";
    let make = |changed: bool| {
        let mut data = lock(if changed { 2 } else { 1 }, 1, false)
            .project()
            .unwrap()
            .clone();
        let other = lock(if changed { 4 } else { 3 }, 1, false)
            .project()
            .unwrap()
            .clone();
        for section in ["packages", "snapshots"] {
            data[section]
                .as_object_mut()
                .unwrap()
                .extend(other[section].as_object().unwrap().clone());
        }
        let left = data["importers"]["."].clone();
        data["importers"] = json!({".":{},"left":left,"right":other["importers"]["."]});
        if !changed {
            data["packages"]["child@1.0.0"] = json!({"resolution":{"integrity":"sha512-child"}});
            data["snapshots"][format!("child@{opaque}")] = json!({
                "dependencies":{"left":"1.0.0(a@1.0.0)","right":"1.0.0(a@3.0.0)"},
                "transitivePeerDependencies":["a"]
            });
            for (name, a) in [("left", 1), ("right", 3)] {
                data["packages"][format!("{name}@1.0.0")] = json!({"resolution":{"integrity":format!("sha512-{name}")},"peerDependencies":{"a":"*"}});
                data["snapshots"][format!("{name}@1.0.0(a@{a}.0.0)")] =
                    json!({"dependencies":{"a":format!("{a}.0.0")}});
                data["snapshots"][format!("lib@1.0.0(a@{a}.0.0)(b@1.0.0)")]["dependencies"]["child"] =
                    json!(opaque);
            }
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let input = make(false);
    let ours = make(true);
    let config = cfg(&json!({"resolvePeersFromWorkspaceRoot":false}));
    let manifests = BTreeMap::from([
        (".".into(), json!({})),
        (
            "left".into(),
            json!({"dependencies":{"a":"2.0.0","b":"1.0.0","lib":"1.0.0"}}),
        ),
        (
            "right".into(),
            json!({"dependencies":{"a":"4.0.0","b":"1.0.0","lib":"1.0.0"}}),
        ),
    ]);
    let result = merge(
        &input,
        &ours,
        &input,
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
    result.validate().unwrap();
    let data = result.project().unwrap();
    for (workspace, a, left, right) in [("left", 2, 2, 3), ("right", 4, 1, 4)] {
        let hash: String = Sha256::digest(format!("a@{a}.0.0").as_bytes())[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let child = format!("1.0.0({hash})");
        let lib = data["importers"][workspace]["dependencies"]["lib"]["version"]
            .as_str()
            .unwrap();
        assert_eq!(
            data["snapshots"][format!("lib@{lib}")]["dependencies"]["child"],
            child
        );
        for (name, value) in [("left", left), ("right", right)] {
            assert_eq!(
                data["snapshots"][format!("child@{child}")]["dependencies"][name],
                format!("1.0.0(a@{value}.0.0)")
            );
        }
    }
}

#[test]
fn opaque_children_require_edge_or_enclosing_context_evidence() {
    use sha2::{Digest, Sha256};
    for child_peer in ["z", "a", "ambiguous", "unbound"] {
        let mut data = lock(1, 1, false).project().unwrap().clone();
        let opaque = "1.0.0(ffffffffffffffffffffffffffffffff)";
        data["packages"]["child@1.0.0"] =
            json!({"resolution":{"integrity":"sha512-child"},"peerDependencies":{child_peer:"*"}});
        data["packages"]["z@1.0.0"] = json!({"resolution":{"integrity":"sha512-z"}});
        data["snapshots"]["z@1.0.0"] = json!({});
        data["snapshots"][format!("child@{opaque}")] = json!({"dependencies":{child_peer:"1.0.0"}});
        if matches!(child_peer, "ambiguous" | "unbound") {
            let peer = if child_peer == "unbound" { "c" } else { "a" };
            data["packages"]["child@1.0.0"]
                .as_object_mut()
                .unwrap()
                .remove("peerDependencies");
            data["snapshots"][format!("child@{opaque}")] = json!({"dependencies":{"left":format!("1.0.0({peer}@1.0.0)"),"right":format!("1.0.0({peer}@3.0.0)")},"transitivePeerDependencies":[peer]});
            if child_peer == "unbound" {
                data["packages"]["child@1.0.0"]["peerDependencies"] = json!({"a":"*"});
                data["snapshots"][format!("child@{opaque}")]["dependencies"]["a"] = json!("1.0.0");
            }
            for (name, a) in [("left", 1), ("right", 3)] {
                data["packages"][format!("{peer}@{a}.0.0")] =
                    json!({"resolution":{"integrity":format!("sha512-{peer}{a}")}});
                data["snapshots"][format!("{peer}@{a}.0.0")] = json!({});
                data["packages"][format!("{name}@1.0.0")] =
                    json!({"resolution":{"integrity":"sha512-leaf"},"peerDependencies":{peer:"*"}});
                data["snapshots"][format!("{name}@1.0.0({peer}@{a}.0.0)")] =
                    json!({"dependencies":{peer:format!("{a}.0.0")}});
            }
        }
        data["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["child"] = json!(opaque);
        data["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["z"] = json!("1.0.0");
        let input = Lockfile::parse(&data.to_string()).unwrap();
        let ours = lock(2, 1, false);
        let config = WorkspaceConfig::default();
        let manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"a":"2.0.0","b":"1.0.0","lib":"1.0.0"}}),
        )]);
        let patches = BTreeMap::new();
        let result = merge(
            &input,
            &ours,
            &input,
            Some(&MergeContext {
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&config; 3],
                configuration: &config,
                patches: &patches,
                registry: None,
            }),
        );
        if child_peer == "unbound" {
            assert!(matches!(
                result,
                Err(LockfileError::ResolutionRequired { .. })
            ));
        } else {
            let result = result.unwrap();
            result.validate().unwrap();
            let reconstructed = format!(
                "1.0.0({})",
                Sha256::digest(b"a@2.0.0")[..16]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            let expected = if matches!(child_peer, "a" | "ambiguous") {
                reconstructed.as_str()
            } else {
                opaque
            };
            assert_eq!(
                result.project().unwrap()["snapshots"]["lib@1.0.0(a@2.0.0)(b@1.0.0)"]["dependencies"]
                    ["child"],
                expected
            );
            if child_peer == "ambiguous" {
                assert_eq!(
                    result.project().unwrap()["snapshots"][format!("child@{expected}")]["dependencies"],
                    json!({"left":"1.0.0(a@2.0.0)","right":"1.0.0(a@3.0.0)"})
                );
            } else {
                assert_eq!(
                    result.project().unwrap()["snapshots"][format!("child@{expected}")]["dependencies"]
                        [child_peer],
                    if child_peer == "a" { "2.0.0" } else { "1.0.0" }
                );
            }
        }
    }
}

#[test]
fn unchanged_opaque_descendants_survive_new_peer_requirements() {
    let opaque = "1.0.0(ffffffffffffffffffffffffffffffff)";
    let make = |wrapper: u8, runtime: u8| {
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "overrides":{"runtime":format!("{runtime}.0.0")},
            "importers":{".":{"dependencies":{
                "wrapper":{"specifier":format!("{wrapper}.0.0"),"version":format!("{wrapper}.0.0(c@1.0.0)")},
                "c":{"specifier":"1.0.0","version":"1.0.0"}
            }}},
            "packages":{
                format!("wrapper@{wrapper}.0.0"):{"resolution":{"integrity":"sha512-wrapper"}},
                format!("runtime@{runtime}.0.0"):{"resolution":{"integrity":"sha512-runtime"}},
                format!("branch@{wrapper}.0.0"):{"resolution":{"integrity":"sha512-branch"}},
                "opaque@1.0.0":{"resolution":{"integrity":"sha512-opaque"},"peerDependencies":{"c":"*"}},
                "c@1.0.0":{"resolution":{"integrity":"sha512-c"}}
            },
            "snapshots":{
                format!("wrapper@{wrapper}.0.0(c@1.0.0)"):{"dependencies":{"opaque":opaque,"branch":format!("{wrapper}.0.0")},"transitivePeerDependencies":["c"]},
                format!("branch@{wrapper}.0.0"):{"dependencies":{"runtime":format!("{runtime}.0.0")}},
                format!("runtime@{runtime}.0.0"):{},
                format!("opaque@{opaque}"):{"dependencies":{"c":"1.0.0"}},
                "c@1.0.0":{}
            }
        });
        if runtime == 2 {
            data["packages"]["runtime@2.0.0"]["peerDependencies"] = json!({"c":"*"});
            data["snapshots"]
                .as_object_mut()
                .unwrap()
                .remove("runtime@2.0.0");
            data["snapshots"]["runtime@2.0.0(c@1.0.0)"] = json!({"dependencies":{"c":"1.0.0"}});
            data["snapshots"]
                .as_object_mut()
                .unwrap()
                .remove(&format!("branch@{wrapper}.0.0"));
            data["snapshots"][format!("branch@{wrapper}.0.0(c@1.0.0)")] = json!({"dependencies":{"runtime":"2.0.0(c@1.0.0)"},"transitivePeerDependencies":["c"]});
            data["snapshots"][format!("wrapper@{wrapper}.0.0(c@1.0.0)")]["dependencies"]["branch"] =
                json!(format!("{wrapper}.0.0(c@1.0.0)"));
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = cfg(&json!({"overrides":{"runtime":"1.0.0"}}));
    let after = cfg(&json!({"overrides":{"runtime":"2.0.0"}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"wrapper":"2.0.0","c":"1.0.0"}}),
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
        data["snapshots"]["wrapper@2.0.0(c@1.0.0)"]["dependencies"]["opaque"],
        opaque
    );
    assert_eq!(
        data["snapshots"]["branch@2.0.0(c@1.0.0)"]["dependencies"]["runtime"],
        "2.0.0(c@1.0.0)"
    );
}

#[test]
fn converging_instances_prefer_the_source_with_fewer_new_peer_requirements() {
    let make = |release: u8, upgraded: bool| {
        let names = if upgraded {
            "(a@1.0.0)(b@1.0.0)(c@1.0.0)"
        } else {
            "(a@1.0.0)(b@1.0.0)"
        };
        let left = if upgraded { names } else { "(a@1.0.0)" };
        let right = names;
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers":{".":{"dependencies":{
                "wrapper":{"specifier":format!("{release}.0.0"),"version":format!("{release}.0.0{names}")},
                "a":{"specifier":"1.0.0","version":"1.0.0"},
                "b":{"specifier":"1.0.0","version":"1.0.0"},
                "c":{"specifier":"1.0.0","version":"1.0.0"}
            }}},
            "packages":{
                format!("wrapper@{release}.0.0"):{"resolution":{"integrity":"sha512-wrapper"}},
                format!("lib@{release}.0.0"):{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"a":"*"}},
                "runtime@1.0.0":{"resolution":{"integrity":"sha512-runtime1"}},
                "runtime@2.0.0":{"resolution":{"integrity":"sha512-runtime2"},"peerDependencies":{"b":"*"}},
                "runtime@3.0.0":{"resolution":{"integrity":"sha512-runtime3"},"peerDependencies":{"b":"*","c":"*"}}
            },
            "snapshots":{
                format!("wrapper@{release}.0.0{names}"):{"dependencies":{"left":format!("lib@{release}.0.0{left}"),"right":format!("lib@{release}.0.0{right}")},"transitivePeerDependencies":if upgraded { json!(["a","b","c"]) } else { json!(["a","b"]) }},
                "runtime@1.0.0":{},
                "runtime@2.0.0(b@1.0.0)":{"dependencies":{"b":"1.0.0"}},
                "runtime@3.0.0(b@1.0.0)(c@1.0.0)":{"dependencies":{"b":"1.0.0","c":"1.0.0"}}
            }
        });
        for name in ["a", "b", "c", "util"] {
            for version in [1, 2] {
                data["packages"][format!("{name}@{version}.0.0")] =
                    json!({"resolution":{"integrity":format!("sha512-{name}{version}")}});
                data["snapshots"][format!("{name}@{version}.0.0")] = json!({});
            }
        }
        for (suffix, runtime, util) in [
            (
                left,
                if upgraded {
                    "3.0.0(b@1.0.0)(c@1.0.0)"
                } else {
                    "1.0.0"
                },
                if upgraded { 2 } else { 1 },
            ),
            (
                right,
                if upgraded {
                    "3.0.0(b@1.0.0)(c@1.0.0)"
                } else {
                    "2.0.0(b@1.0.0)"
                },
                2,
            ),
        ] {
            let key = format!("lib@{release}.0.0{suffix}");
            data["snapshots"][&key] = json!({"dependencies":{"a":"1.0.0","runtime":runtime,"util":format!("{util}.0.0")}});
            if suffix.contains("(b@") {
                data["snapshots"][&key]["transitivePeerDependencies"] = if upgraded {
                    json!(["b", "c"])
                } else {
                    json!(["b"])
                };
            }
        }
        if upgraded {
            data["overrides"] = json!({"runtime":"3.0.0"});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = WorkspaceConfig::default();
    let after = cfg(&json!({"overrides":{"runtime":"3.0.0"}}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"wrapper":"2.0.0","a":"2.0.0","b":"2.0.0","c":"2.0.0"}}),
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
    let data = result.project().unwrap();
    let suffix = "(a@2.0.0)(b@2.0.0)(c@2.0.0)";
    for name in ["left", "right"] {
        assert_eq!(
            data["snapshots"][format!("wrapper@2.0.0{suffix}")]["dependencies"][name],
            format!("lib@2.0.0{suffix}")
        );
    }
    assert_eq!(
        data["snapshots"][format!("lib@2.0.0{suffix}")]["dependencies"]["util"],
        "2.0.0"
    );
    result.validate().unwrap();
}

#[test]
fn converging_instances_preserve_the_closest_existing_provider_versions() {
    let make = |release: u8, upgraded: bool| {
        let runtime = if upgraded { 2 } else { 1 };
        let mut data = json!({
            "lockfileVersion":"9.0",
            "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "overrides":{"runtime":format!("{runtime}.0.0")},
            "importers":{".":{}}, "packages":{}, "snapshots":{}
        });
        for a in [1, 2] {
            let suffix = format!("(a@{a}.0.0){}", if upgraded { "(b@1.0.0)" } else { "" });
            data["importers"][format!("apps/{a}")] = json!({"dependencies":{
                "lib":{"specifier":format!("{release}.0.0"),"version":format!("{release}.0.0{suffix}")},
                "a":{"specifier":format!("{a}.0.0"),"version":format!("{a}.0.0")},
                "b":{"specifier":"1.0.0","version":"1.0.0"}
            }});
            data["packages"][format!("lib@{release}.0.0")] =
                json!({"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"a":"*"}});
            let key = format!("lib@{release}.0.0{suffix}");
            data["snapshots"][&key] = json!({"dependencies":{
                "a":format!("{a}.0.0"), "util":format!("{a}.0.0"),
                "runtime":format!("{runtime}.0.0{}",if upgraded { "(b@1.0.0)" } else { "" })
            }});
            if upgraded {
                data["snapshots"][&key]["transitivePeerDependencies"] = json!(["b"]);
            }
            for name in ["a", "b", "util"] {
                data["packages"][format!("{name}@{a}.0.0")] =
                    json!({"resolution":{"integrity":format!("sha512-{name}{a}")}});
                data["snapshots"][format!("{name}@{a}.0.0")] = json!({});
            }
        }
        data["packages"][format!("runtime@{runtime}.0.0")] =
            json!({"resolution":{"integrity":format!("sha512-runtime{runtime}")}});
        if upgraded {
            data["packages"]["runtime@2.0.0"]["peerDependencies"] = json!({"b":"*"});
            data["snapshots"]["runtime@2.0.0(b@1.0.0)"] = json!({"dependencies":{"b":"1.0.0"}});
        } else {
            data["snapshots"]["runtime@1.0.0"] = json!({});
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let before = cfg(&json!({"overrides":{"runtime":"1.0.0"}}));
    let after = cfg(&json!({"overrides":{"runtime":"2.0.0"}}));
    let mut manifests: BTreeMap<_, _> = [1, 2]
        .into_iter()
        .map(|id| {
            (
                format!("apps/{id}"),
                json!({"dependencies":{"lib":"2.0.0","a":"2.0.0","b":"2.0.0"}}),
            )
        })
        .collect();
    manifests.insert(".".into(), json!({}));
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
    let data = result.project().unwrap();
    for id in [1, 2] {
        assert_eq!(
            data["importers"][format!("apps/{id}")]["dependencies"]["lib"]["version"],
            "2.0.0(a@2.0.0)(b@2.0.0)"
        );
    }
    assert_eq!(
        data["snapshots"]["lib@2.0.0(a@2.0.0)(b@2.0.0)"]["dependencies"]["util"],
        "2.0.0"
    );
    result.validate().unwrap();
}

#[test]
fn opaque_flat_contexts_accept_enclosing_providers_absent_from_descendant_bindings() {
    use sha2::{Digest, Sha256};
    let make = |b| {
        let mut data = lock(5, b, false).project().unwrap().clone();
        data["settings"]["dedupePeers"] = json!(true);
        if b == 1 {
            let child = "1.0.0(ffffffffffffffffffffffffffffffff)";
            data["packages"]["child@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-child"},"peerDependencies":{"b":"*"}});
            data["snapshots"][format!("child@{child}")] = json!({"dependencies":{"b":"1.0.0","left":"1.0.0(a@1.0.0)","right":"1.0.0(a@3.0.0)"},"transitivePeerDependencies":["a"]});
            data["snapshots"]["lib@1.0.0(a@5.0.0)(b@1.0.0)"]["dependencies"]["child"] =
                json!(child);
            for (name, a) in [("left", 1), ("right", 3)] {
                data["packages"][format!("a@{a}.0.0")] =
                    json!({"resolution":{"integrity":format!("sha512-a{a}")}});
                data["snapshots"][format!("a@{a}.0.0")] = json!({});
                data["packages"][format!("{name}@1.0.0")] = json!({"resolution":{"integrity":format!("sha512-{name}")},"peerDependencies":{"a":"*"}});
                data["snapshots"][format!("{name}@1.0.0(a@{a}.0.0)")] =
                    json!({"dependencies":{"a":format!("{a}.0.0")}});
            }
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let config = cfg(&json!({"dedupePeers":true}));
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"lib":"1.0.0","a":"5.0.0","b":"2.0.0"}}),
    )]);
    let result = merge(
        &make(1),
        &make(2),
        &make(1),
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
    let hash: String = Sha256::digest(b"a@5.0.0)(b@2.0.0")[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let child = format!("1.0.0({hash})");
    let data = result.project().unwrap();
    assert_eq!(
        data["snapshots"]["lib@1.0.0(a@5.0.0)(b@2.0.0)"]["dependencies"]["child"],
        child
    );
    assert_eq!(
        data["snapshots"][format!("child@{child}")]["dependencies"],
        json!({"b":"2.0.0","left":"1.0.0(a@1.0.0)","right":"1.0.0(a@3.0.0)"})
    );
    result.validate().unwrap();
}

#[test]
fn cyclic_peers_keep_their_back_edges_short_and_their_shared_provider_qualified() {
    let make = |adapter: &str, engine: &str| {
        let platform = format!("1.0.0(engine@{engine})");
        let adapter_id = format!("{adapter}(platform@{platform})(runner@1.0.0)");
        let runner_id = format!("1.0.0(adapter@{adapter})(platform@{platform})");
        let doc = json!({
            "lockfileVersion":"9.0", "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
            "importers":{".":{"dependencies":{
                "engine":{"specifier":"*","version":engine}, "platform":{"specifier":"*","version":platform},
                "adapter":{"specifier":"*","version":adapter_id}, "runner":{"specifier":"*","version":runner_id}
            }}},
            "packages":{
                format!("engine@{engine}"):{"resolution":{"integrity":"sha512-engine"}},
                "platform@1.0.0":{"resolution":{"integrity":"sha512-platform"},"peerDependencies":{"engine":"*"}},
                format!("adapter@{adapter}"):{"resolution":{"integrity":"sha512-adapter"},"peerDependencies":{"platform":"*","runner":"*"}},
                "runner@1.0.0":{"resolution":{"integrity":"sha512-runner"},"peerDependencies":{"adapter":"*","platform":"*"}}
            },
            "snapshots":{
                format!("engine@{engine}"): {},
                format!("platform@{platform}"): {"dependencies":{"engine":engine}},
                format!("adapter@{adapter_id}"): {"dependencies":{"platform":platform,"runner":runner_id}},
                format!("runner@{runner_id}"): {"dependencies":{"adapter":adapter_id,"platform":platform}}
            }
        });
        Lockfile::parse(&doc.to_string()).unwrap()
    };
    let config = WorkspaceConfig::default();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{
            "adapter":"2.0.0","engine":"2.0.0","platform":"*","runner":"*"
        }}),
    )]);
    let result = merge(
        &make("1.0.0", "1.0.0"),
        &make("2.0.0", "1.0.0"),
        &make("1.0.0", "2.0.0"),
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
    let doc = result.project().unwrap();
    let adapter = "2.0.0(platform@1.0.0(engine@2.0.0))(runner@1.0.0)";
    let runner = "1.0.0(adapter@2.0.0)(platform@1.0.0(engine@2.0.0))";
    assert_eq!(
        doc["importers"]["."]["dependencies"]["adapter"]["version"],
        adapter
    );
    assert_eq!(
        doc["snapshots"][format!("adapter@{adapter}")]["dependencies"]["runner"],
        runner
    );
    assert_eq!(
        doc["snapshots"][format!("runner@{runner}")]["dependencies"]["adapter"],
        adapter
    );
    result.validate().unwrap();
}
