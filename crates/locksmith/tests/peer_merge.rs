use locksmith::{Lockfile, LockfileError, Value, merge};
use serde_json::json;

fn fixture(a: u8, b: u8, nested: bool) -> Lockfile {
    let peers = format!("(a@{a}.0.0)(b@{b}.0.0)");
    let mut snapshots = json!({
        format!("a@{a}.0.0"): {}, format!("b@{b}.0.0"): {},
        format!("lib@1.0.0{peers}"): {"dependencies":{"a":format!("{a}.0.0"),"b":format!("{b}.0.0")}}
    });
    let mut packages = json!({
        format!("a@{a}.0.0"): {"resolution":{"integrity":"sha512-a"}},
        format!("b@{b}.0.0"): {"resolution":{"integrity":"sha512-b"}},
        "lib@1.0.0": {"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"a":"*","b":"*"}}
    });
    let name = if nested {
        snapshots[format!("wrapper@1.0.0{peers}")] = json!({"dependencies":{"lib":format!("1.0.0{peers}")},"transitivePeerDependencies":["a","b"]});
        packages["wrapper@1.0.0"] = json!({"resolution":{"integrity":"sha512-wrapper"}});
        "wrapper"
    } else {
        "lib"
    };
    Lockfile::parse(
        &json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{
        name:{"specifier":"1.0.0","version":format!("1.0.0{peers}")},
        "a":{"specifier":format!("{a}.0.0"),"version":format!("{a}.0.0")},
        "b":{"specifier":format!("{b}.0.0"),"version":format!("{b}.0.0")}
    }}},"packages":packages,"snapshots":snapshots})
        .to_string(),
    )
    .unwrap()
}

#[test]
fn independent_peer_updates_compose_and_installable_edges_are_preserved() {
    for nested in [false, true] {
        let base = fixture(1, 1, nested);
        let ours = fixture(2, 1, nested);
        let theirs = fixture(1, 2, nested);
        let expected = fixture(2, 2, nested);
        for (ours, theirs) in [(&ours, &theirs), (&theirs, &ours)] {
            let result = merge(&base, ours, theirs, None).unwrap();
            result.validate().unwrap();
            assert_eq!(result.project(), expected.project());
            assert_eq!(
                Lockfile::parse(&result.to_yaml().unwrap())
                    .unwrap()
                    .project(),
                expected.project()
            );
        }
    }
}

#[test]
fn competing_versions_of_the_same_peer_remain_a_conflict() {
    assert!(matches!(
        merge(
            &fixture(1, 1, false),
            &fixture(2, 1, false),
            &fixture(3, 1, false),
            None
        ),
        Err(LockfileError::Conflicts(_))
    ));
}

#[test]
fn flattened_peer_upgrades_restore_each_consumers_nested_provider_context() {
    use sha2::{Digest, Sha256};
    let make = |release: u8, provider: u8| {
        let hash: String = Sha256::digest(format!("provider@{provider}.0.0").as_bytes())[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let wrapper = format!("wrapper@{release}.0.0({hash})");
        let mut data = json!({"lockfileVersion":"9.0",
            "importers":{".":{"dependencies":{"wrapper":{"specifier":"*","version":format!("{release}.0.0({hash})")}}}},
            "packages":{
                format!("wrapper@{release}.0.0"):{"resolution":{"integrity":format!("sha512-wrapper-{release}")}},
                format!("provider@{provider}.0.0"):{"resolution":{"integrity":format!("sha512-provider-{provider}")},"peerDependencies":{"x":"*"}}
            },
            "snapshots":{&wrapper:{"dependencies":{},"transitivePeerDependencies":["provider"]}}
        });
        for (consumer, x) in [("left", 1), ("right", 2)] {
            data["packages"][format!("x@{x}.0.0")] =
                json!({"resolution":{"integrity":format!("sha512-x-{x}")}});
            data["snapshots"][format!("x@{x}.0.0")] = json!({});
            data["snapshots"][format!("provider@{provider}.0.0(x@{x}.0.0)")] =
                json!({"dependencies":{"x":format!("{x}.0.0")}});
            data["packages"][format!("{consumer}@{release}.0.0")] = json!({"resolution":{"integrity":format!("sha512-{consumer}-{release}")},"peerDependencies":{"provider":"*"}});
            let reference = format!("{release}.0.0(provider@{provider}.0.0)");
            data["snapshots"][format!("{consumer}@{reference}")] =
                json!({"dependencies":{"provider":format!("{provider}.0.0(x@{x}.0.0)")}});
            data["snapshots"][&wrapper]["dependencies"][consumer] = json!(reference);
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, 1);
    let ours = make(1, 2);
    let theirs = make(2, 1);
    for (ours, theirs) in [(&ours, &theirs), (&theirs, &ours)] {
        let output = merge(&base, ours, theirs, None).unwrap();
        output.validate().unwrap();
        for (consumer, x) in [("left", 1), ("right", 2)] {
            assert_eq!(
                output.project().unwrap()["snapshots"][format!("{consumer}@2.0.0(provider@2.0.0)")]
                    ["dependencies"]["provider"],
                format!("2.0.0(x@{x}.0.0)")
            );
        }
        assert_eq!(output.project(), make(2, 2).project());
    }
}

#[test]
fn combining_peer_contexts_reports_ranges_without_deciding_acceptance() {
    let restrict = |lock: Lockfile| {
        let mut data = lock.project().unwrap().clone();
        data["packages"]["lib@1.0.0"]["peerDependencies"]["a"] = json!("^1");
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let output = merge(
        &restrict(fixture(1, 1, false)),
        &restrict(fixture(2, 1, false)),
        &restrict(fixture(1, 2, false)),
        None,
    )
    .unwrap();
    output.validate().unwrap();
    let issues = locksmith::inspect_peers(&output, &locksmith::PeerRules::default(), None);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].peer, "a");
    assert_eq!(issues[0].wanted, "^1");
    assert_eq!(issues[0].resolved.as_deref(), Some("2.0.0"));
}

#[test]
fn snapshot_dependency_edges_merge_independently() {
    let make = |a: u8, b: u8| {
        let mut data: Value = fixture(a, b, false).project().unwrap().clone();
        data["packages"]["lib@1.0.0"]
            .as_object_mut()
            .unwrap()
            .remove("peerDependencies");
        let key = format!("lib@1.0.0(a@{a}.0.0)(b@{b}.0.0)");
        let record = data["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove(&key)
            .unwrap();
        data["snapshots"]["lib@1.0.0"] = record;
        data["importers"]["."]["dependencies"]["lib"]["version"] = json!("1.0.0");
        Lockfile::parse(&data.to_string()).unwrap()
    };
    assert_eq!(
        merge(&make(1, 1), &make(2, 1), &make(1, 2), None)
            .unwrap()
            .project(),
        make(2, 2).project()
    );
}

#[test]
fn unchanged_peer_policy_exceptions_survive_independent_dependency_updates() {
    let make = |x: u8, y: u8| {
        let mut data = fixture(1, 1, false).project().unwrap().clone();
        // pnpm can retain a binding outside the package's declared range under
        // the workspace's peer policy. Neither branch changes that binding.
        data["packages"]["lib@1.0.0"]["peerDependencies"]["a"] = json!("^2");
        for (name, version) in [("runtime-x", x), ("runtime-y", y)] {
            let key = format!("{name}@{version}.0.0");
            data["packages"][&key] = json!({"resolution":{"integrity":"sha512-runtime"}});
            data["snapshots"][&key] = json!({});
            data["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"][name] =
                json!(format!("{version}.0.0"));
        }
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, 1);
    let ours = make(2, 1);
    let theirs = make(1, 2);
    for (o, t) in [(&ours, &theirs), (&theirs, &ours)] {
        let output = merge(&base, o, t, None).unwrap();
        output.validate().unwrap();
        assert_eq!(output.project(), make(2, 2).project());
    }
}

#[test]
fn release_upgrade_rebinds_peers_without_copying_old_release_dependencies() {
    let base = fixture(1, 1, false);
    let mut upgraded = base.project().unwrap().clone();
    let mut package = upgraded["packages"]
        .as_object_mut()
        .unwrap()
        .remove("lib@1.0.0")
        .unwrap();
    package["resolution"]["integrity"] = json!("sha512-new-release");
    upgraded["packages"]["lib@2.0.0"] = package;
    upgraded["packages"]["new-runtime@2.0.0"] =
        json!({"resolution":{"integrity":"sha512-runtime"}});
    let mut snapshot = upgraded["snapshots"]
        .as_object_mut()
        .unwrap()
        .remove("lib@1.0.0(a@1.0.0)(b@1.0.0)")
        .unwrap();
    snapshot["dependencies"]["new-runtime"] = json!("2.0.0");
    upgraded["snapshots"]["lib@2.0.0(a@1.0.0)(b@1.0.0)"] = snapshot;
    upgraded["snapshots"]["new-runtime@2.0.0"] = json!({});
    // The range permits either release; one branch updates the locked release.
    let normalize = |mut value: Value| {
        value["importers"]["."]["dependencies"]["lib"]["specifier"] = json!(">=1");
        Lockfile::parse(&value.to_string()).unwrap()
    };
    upgraded["importers"]["."]["dependencies"]["lib"]["version"] = json!("2.0.0(a@1.0.0)(b@1.0.0)");
    let ours = normalize(upgraded);
    let theirs = normalize(fixture(1, 2, false).project().unwrap().clone());
    let base = normalize(base.project().unwrap().clone());
    for (o, t) in [(&ours, &theirs), (&theirs, &ours)] {
        let merged = merge(&base, o, t, None).unwrap();
        merged.validate().unwrap();
        assert_eq!(
            merged.project().unwrap()["importers"]["."]["dependencies"]["lib"]["version"],
            "2.0.0(a@1.0.0)(b@2.0.0)"
        );
        assert_eq!(
            merged.project().unwrap()["snapshots"]["lib@2.0.0(a@1.0.0)(b@2.0.0)"]["dependencies"]["new-runtime"],
            "2.0.0"
        );
    }
}

#[test]
fn concurrent_release_upgrades_preserve_disagreement_in_new_regular_dependencies() {
    let make = |release: u8, a: u8, b: u8, runtime: u8| {
        let original = fixture(a, b, false);
        let data = original.project().unwrap().to_string();
        let mut data: Value =
            serde_json::from_str(&data.replace("lib@1.0.0", &format!("lib@{release}.0.0")))
                .unwrap();
        let peers = format!("(a@{a}.0.0)(b@{b}.0.0)");
        data["importers"]["."]["dependencies"]["lib"] =
            json!({"specifier":">=1", "version":format!("{release}.0.0{peers}")});
        data["snapshots"][format!("lib@{release}.0.0{peers}")]["dependencies"]["runtime"] =
            json!(format!("{runtime}.0.0"));
        data["snapshots"][format!("runtime@{runtime}.0.0")] = json!({});
        data["packages"][format!("runtime@{runtime}.0.0")] =
            json!({"resolution":{"integrity":"sha512-runtime"}});
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = make(1, 1, 1, 1);
    let ours = make(2, 2, 1, 2);
    // Check both a newly composed context and a context already present in ours.
    for b in [1, 2] {
        let theirs = make(2, 1, b, 3);
        for (o, t) in [(&ours, &theirs), (&theirs, &ours)] {
            assert!(matches!(
                merge(&base, o, t, None),
                Err(LockfileError::Conflicts(_))
            ));
        }
    }
}

#[test]
fn composing_a_peer_context_does_not_rebind_an_unrelated_consumer() {
    let add_sibling = |lock: Lockfile| {
        let mut data = lock.project().unwrap().clone();
        let old = fixture(1, 1, false);
        for section in ["packages", "snapshots"] {
            for (k, v) in old.project().unwrap()[section].as_object().unwrap() {
                data[section]
                    .as_object_mut()
                    .unwrap()
                    .entry(k.clone())
                    .or_insert(v.clone());
            }
        }
        data["importers"]["sibling"] = old.project().unwrap()["importers"]["."].clone();
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = add_sibling(fixture(1, 1, false));
    let result = merge(
        &base,
        &add_sibling(fixture(2, 1, false)),
        &add_sibling(fixture(1, 2, false)),
        None,
    )
    .unwrap();
    assert_eq!(
        result.project().unwrap()["importers"]["sibling"],
        base.project().unwrap()["importers"]["sibling"]
    );
    assert_eq!(
        result.project().unwrap()["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"],
        base.project().unwrap()["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]
    );
    result.validate().unwrap();
}

#[test]
fn hashed_transitive_peer_contexts_are_recovered_and_verified() {
    // Digests of `a@<version>)(b@<version>` using pnpm's documented encodings.
    for hashes in [
        [
            "bff246112e0e73444993332af97741a4",
            "9ce283e4780c68eed3dd89aeb9cb67a4",
            "1b449cb0abeb74747f6b778d225da4b0",
            "06ce7e7c3b17c0697fd17d45286369ad",
        ],
        [
            "h5uilbdwtlrvzj73h5lvtstrnq",
            "ph4cd7vua66lktdwx2jwaz4tey",
            "fb6eftq7xntee3bct3ywlf6lua",
            "625obrdbhgenqqep3ckia5awtq",
        ],
    ] {
        let make = |a, b, hash| {
            Lockfile::parse(
                &fixture(a, b, true)
                    .project()
                    .unwrap()
                    .to_string()
                    .replace(&format!("(a@{a}.0.0)(b@{b}.0.0)"), &format!("({hash})")),
            )
            .unwrap()
        };
        let base = make(1, 1, hashes[0]);
        let ours = make(2, 1, hashes[1]);
        let theirs = make(1, 2, hashes[2]);
        for (o, t) in [(&ours, &theirs), (&theirs, &ours)] {
            let output = merge(&base, o, t, None).unwrap();
            output.validate().unwrap();
            assert_eq!(output.project(), make(2, 2, hashes[3]).project());
        }
        let invalid = make(1, 1, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(matches!(
            merge(&invalid, &ours, &theirs, None),
            Err(LockfileError::Conflicts(_))
        ));
    }
}

#[test]
fn peer_identities_sort_complete_names_and_ignore_descendant_local_providers() {
    use sha2::{Digest, Sha256};

    for hashed in [false, true] {
        let make = |a: u8, b: u8| {
            // pnpm's identity order includes the @ separator: '-' precedes '@'.
            let peer_ids = format!("runtime-helper@{b}.0.0)(runtime@{a}.0.0");
            let suffix = if hashed {
                let digest: String = Sha256::digest(peer_ids.as_bytes())[..16]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                format!("({digest})")
            } else {
                format!("({peer_ids})")
            };
            let original = fixture(a, b, true).project().unwrap().to_string();
            let original = original
                .replace(&format!("(a@{a}.0.0)(b@{b}.0.0)"), &suffix)
                .replace("\"a\"", "\"runtime\"")
                .replace("\"b\"", "\"runtime-helper\"")
                .replace("\"a@", "\"runtime@")
                .replace("\"b@", "\"runtime-helper@");
            let mut data: Value = serde_json::from_str(&original).unwrap();
            data["packages"]["private@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-private"}});
            data["packages"]["runtime@9.0.0"] =
                json!({"resolution":{"integrity":"sha512-private-runtime"}});
            data["snapshots"]["runtime@9.0.0"] = json!({});
            data["snapshots"]["private@1.0.0"] = json!({"dependencies":{"runtime":"9.0.0"}});
            data["snapshots"][format!("wrapper@1.0.0{suffix}")]["dependencies"]["private"] =
                json!("1.0.0");
            Lockfile::parse(&data.to_string()).unwrap()
        };
        let base = make(1, 1);
        let ours = make(2, 1);
        let theirs = make(1, 2);
        for (o, t) in [(&ours, &theirs), (&theirs, &ours)] {
            let output = merge(&base, o, t, None).unwrap();
            output.validate().unwrap();
            assert_eq!(output.project(), make(2, 2).project());
        }
    }
}

#[test]
fn transitive_aliases_use_declared_names_through_multiple_wrappers() {
    use sha2::{Digest, Sha256};
    for hashed in [false, true] {
        let make = |a: u8, b: u8| {
            let mut data = fixture(a, b, true).project().unwrap().clone();
            for section in ["packages", "snapshots"] {
                let value = data[section]
                    .as_object_mut()
                    .unwrap()
                    .remove(&format!("b@{b}.0.0"))
                    .unwrap();
                data[section][format!("actual-b@{b}.0.0")] = value;
            }
            data["importers"]["."]["dependencies"]["b"] = json!({"specifier":format!("npm:actual-b@{b}.0.0"),"version":format!("actual-b@{b}.0.0")});
            data["snapshots"][format!("lib@1.0.0(a@{a}.0.0)(b@{b}.0.0)")]["dependencies"]["b"] =
                json!(format!("actual-b@{b}.0.0"));
            let peer_ids = format!("a@{a}.0.0)(actual-b@{b}.0.0");
            let hash: String = Sha256::digest(peer_ids.as_bytes())[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let suffix = if hashed {
                format!("({hash})")
            } else {
                format!("({peer_ids})")
            };
            data["packages"]["outer@1.0.0"] = json!({"resolution":{"integrity":"sha512-outer"}});
            data["snapshots"][format!("outer@1.0.0{suffix}")] = json!({"dependencies":{"wrapper":format!("1.0.0{suffix}")},"transitivePeerDependencies":["a","b"]});
            let dependency = data["importers"]["."]["dependencies"]
                .as_object_mut()
                .unwrap()
                .remove("wrapper")
                .unwrap();
            data["importers"]["."]["dependencies"]["outer"] = dependency;
            Lockfile::parse(
                &data
                    .to_string()
                    .replace(&format!("(a@{a}.0.0)(b@{b}.0.0)"), &suffix),
            )
            .unwrap()
        };
        let output = merge(&make(1, 1), &make(2, 1), &make(1, 2), None).unwrap();
        output.validate().unwrap();
        assert_eq!(output.project(), make(2, 2).project());
    }
}
