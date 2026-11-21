use locksmith::{
    Disposition, Lockfile, LockfileError, PeerRules, PeerStatus, PeerVersionMatching,
    UnknownReason, Value, inspect, inspect_peers,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn lock(doc: &Value) -> Lockfile {
    Lockfile::parse(&doc.to_string()).unwrap()
}
fn binding(wanted: &str, pin: &str, package: Value) -> Value {
    let snapshot = format!("plugin@1.0.0(host@{pin})");
    json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"plugin":{"specifier":"1","version":snapshot.strip_prefix("plugin@").unwrap()}}}},
        "packages":{"plugin@1.0.0":{"resolution":{},"peerDependencies":{"host":wanted}},format!("host@{pin}"):package},
        "snapshots":{snapshot:{"dependencies":{"host":pin}},format!("host@{pin}"): {}}})
}

#[test]
fn peer_policy_handles_partial_upper_bounds_exact_prereleases_and_nonregistry_sources() {
    for major in [9, 10, 11, 12] {
        let version = format!("{major}.0.0").parse().unwrap();
        let rules = PeerRules::from_pnpm(&json!({}), PeerVersionMatching::for_pnpm(Some(&version)))
            .unwrap();
        for (range, pin) in [
            (">=0.11 <=15", "15.8.0"),
            (">=0.26 <=0.34", "0.34.33"),
            ("^1", "1.4.0-rc.1"),
        ] {
            assert!(
                inspect_peers(
                    &lock(&binding(range, pin, json!({"resolution":{}}))),
                    &rules,
                    None
                )
                .is_empty(),
                "{major}: {range} with {pin}"
            );
        }
        let bad = lock(&binding(
            "19.0.0-rc.7",
            "19.0.0-rc.4",
            json!({"resolution":{}}),
        ));
        let result = inspect(&bad, &rules, None).unwrap();
        assert_eq!(result.peers[0].requirement.status, PeerStatus::OutOfRange);
        assert_eq!(
            result.peers[0].requirement.disposition,
            Disposition::Rejected
        );
        let local = lock(&binding(
            "^1",
            "file:host.tgz",
            json!({"resolution":{"tarball":"file:host.tgz"},"version":"1.4.0"}),
        ));
        assert!(inspect_peers(&local, &rules, None).is_empty());
        let dist_tag = inspect(
            &lock(&binding("latest", "1.4.0", json!({"resolution":{}}))),
            &rules,
            None,
        )
        .unwrap();
        assert_eq!(dist_tag.peers[0].requirement.status, PeerStatus::Unknown);
        assert_eq!(
            dist_tag.peers[0].requirement.reason,
            Some(UnknownReason::OpaqueSpecifier)
        );
        assert_eq!(
            dist_tag.peers[0].requirement.disposition,
            if major < 11 {
                Disposition::Rejected
            } else {
                Disposition::Accepted
            }
        );
    }
}

#[test]
fn reports_distinct_policy_exceptions_and_unknown_link_versions() {
    let data = binding("^1", "2.0.0", json!({"resolution":{}}));
    for major in [9, 10, 11, 12] {
        let version = format!("{major}.0.0").parse().unwrap();
        let mode = PeerVersionMatching::for_pnpm(Some(&version));
        let rules = PeerRules::from_pnpm(
            &json!({"peerDependencyRules":{"allowedVersions":{"plugin@1>host":"2"}}}),
            mode,
        )
        .unwrap();
        let output = inspect(&lock(&data), &rules, None).unwrap();
        assert_eq!(output.peers[0].requirement.status, PeerStatus::Allowed);
        assert_eq!(
            output.peers[0].requirement.exception.as_deref(),
            Some("allowedVersions/plugin@1>host")
        );
        assert_eq!(
            output.peers[0].requirement.disposition,
            if major == 9 {
                Disposition::Rejected
            } else {
                Disposition::Accepted
            }
        );
    }
    let impossible_exception = PeerRules::from_pnpm(
        &json!({"peerDependencyRules":{"allowedVersions":{"host":">=2.0.0 <1.0.0"}}}),
        PeerVersionMatching::ReleaseCompatible,
    )
    .unwrap();
    assert_eq!(
        inspect(&lock(&data), &impossible_exception, None)
            .unwrap()
            .peers[0]
            .requirement
            .status,
        PeerStatus::OutOfRange
    );
    let unclear = inspect(
        &lock(&binding("latest", "1.0.0-rc.1", json!({"resolution":{}}))),
        &PeerRules::from_pnpm(&json!({}), PeerVersionMatching::Unspecified).unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(
        unclear.peers[0].requirement.reason,
        Some(UnknownReason::PackageManager)
    );
    let mut data = data;
    data["snapshots"]["plugin@1.0.0(host@2.0.0)"]["dependencies"]["host"] = json!("link:linked");
    data["importers"]["linked"] = json!({});
    let lock = lock(&data);
    let policy = PeerRules::default();
    let report = inspect(&lock, &policy, None).unwrap();
    assert_eq!(report.peers[0].requirement.status, PeerStatus::Unknown);
    assert_eq!(
        report.peers[0].requirement.reason,
        Some(UnknownReason::ProviderVersion)
    );
    assert_eq!(
        report.peers[0].requirement.disposition,
        Disposition::Unknown
    );
    assert_eq!(
        report.peers[0].bindings[0].reference.as_deref(),
        Some("link:linked")
    );
    let manifests = BTreeMap::from([("linked".into(), json!({"name":"host","version":"1.0.0"}))]);
    assert!(
        inspect(&lock, &policy, Some(&manifests))
            .unwrap()
            .peers
            .is_empty()
    );
}

#[test]
fn counts_each_workspace_once_across_links_cycles_dev_edges_and_orphans() {
    let dep = |v: &str| json!({"specifier":"*","version":v});
    let mut data = json!({"lockfileVersion":"9.0","importers":{
        "host":{"dependencies":{"leaf":dep("1.0.0")},"devDependencies":{"devtool":dep("1.0.0")}},
        "isolated":{}
    },"packages":{
        "adapter@1.0.0":{"resolution":{}},"leaf@1.0.0":{"resolution":{},"peerDependencies":{"peer":"^1"}},
        "peer@1.5.0":{"resolution":{}}, "devtool@1.0.0":{"resolution":{},"peerDependencies":{"types":"*"}},
        "orphan@1.0.0":{"resolution":{},"peerDependencies":{"gone":"*"}}
    },"snapshots":{"adapter@1.0.0":{"dependencies":{"host":"link:host","sibling":"link:../external"}},
        "leaf@1.0.0":{"dependencies":{"adapter":"1.0.0"}},"devtool@1.0.0":{},"orphan@1.0.0":{},
        "peer@1.5.0":{}}});
    for i in 0..70 {
        data["importers"][format!("apps/{i:02}")] =
            json!({"dependencies":{"adapter":dep("1.0.0")}});
    }
    let input = lock(&data);
    let output = inspect(&input, &PeerRules::default(), None).unwrap();
    let group = |peer: &str| {
        output
            .peers
            .iter()
            .find(|g| g.requirement.peer == peer)
            .unwrap()
    };
    assert_eq!(group("peer").direct_workspaces, ["host"]);
    assert_eq!(group("peer").reachable_workspaces.len(), 71);
    assert_eq!(group("peer").matching_versions_in_lock, ["1.5.0"]);
    assert_eq!(
        group("peer").example_path.as_deref().unwrap(),
        ["host", "leaf@1.0.0"]
    );
    assert_eq!(group("types").reachable_workspaces, ["host"]);
    assert!(group("gone").example_path.is_none());
    assert_eq!(
        output.external_links.iter().next().unwrap().target,
        "../external"
    );
    for g in &output.peers {
        let expected: BTreeSet<_> = data["importers"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|id| {
                let reached = input.dependency_closure(id).unwrap();
                g.bindings
                    .iter()
                    .any(|s| reached.snapshots.contains(&s.snapshot))
            })
            .cloned()
            .collect();
        assert_eq!(expected, g.reachable_workspaces.iter().cloned().collect());
    }
    let from_app = input.dependency_closure("apps/69").unwrap();
    assert!(from_app.importers.contains("host") && from_app.snapshots.contains("leaf@1.0.0"));
    assert!(!from_app.snapshots.contains("devtool@1.0.0"));
}

#[test]
fn rejects_malformed_peer_declarations_with_their_location() {
    for (field, value, tail) in [
        ("peerDependencies", json!([]), vec![]),
        ("peerDependencies", json!({"host":["^1"]}), vec!["host"]),
        (
            "peerDependenciesMeta",
            json!({"host":{"optional":"true"}}),
            vec!["host", "optional"],
        ),
    ] {
        let mut data = binding("^1", "1.0.0", json!({"resolution":{}}));
        data["packages"]["plugin@1.0.0"][field] = value;
        let mut path = vec!["packages", "plugin@1.0.0", field];
        path.extend(tail);
        assert!(
            matches!(Lockfile::parse(&data.to_string()),Err(LockfileError::InvalidGraph(issues)) if issues.iter().any(|i|i.path==path))
        );
    }
}

#[test]
fn comparator_intersections_wildcards_and_prerelease_bounds_hold_in_every_policy() {
    for policy in [
        PeerVersionMatching::Pnpm9,
        PeerVersionMatching::ReleaseCompatible,
        PeerVersionMatching::Pnpm11,
        PeerVersionMatching::Pnpm12,
        PeerVersionMatching::Unspecified,
    ] {
        let rules = PeerRules {
            version_matching: policy,
            ..PeerRules::default()
        };
        for (wanted, version, satisfies) in [
            ("^4.0.0-rc.7", "4.0.0-rc.6", false),
            ("^4.0.0-rc.7", "4.0.0-rc.7", true),
            (">1.0.0-rc.1", "1.0.0-rc.1", false),
            (">1.0.0-rc.1", "1.0.0-alpha.1", false),
            (">1.0.0-rc.1", "1.0.0-rc.2", true),
            (">=2.0.0 <1.0.0", "0.8.0", false),
            (">=2.0.0 <1.0.0", "2.1.0", false),
            (">=2.0.0 <3.0.0", "2.1.0", true),
            (">= 2.0.0 < 1.0.0 || ^4", "2.1.0", false),
            (">=2.0.0 <1.0.0 || ^4", "4.1.0", true),
            (">*", "2.1.0", false),
            ("*", "2.1.0", true),
            (">1", "2.1.0", true),
            ("1.0.0 - 3.0.0", "2.1.0", true),
        ] {
            let report = inspect(
                &lock(&binding(wanted, version, json!({"resolution":{}}))),
                &rules,
                None,
            )
            .unwrap();
            assert_eq!(
                report.peers.is_empty(),
                satisfies,
                "{policy:?}: {version} in {wanted}"
            );
            if !satisfies {
                let g = &report.peers[0];
                assert_eq!(g.requirement.status, PeerStatus::OutOfRange);
                assert_eq!(g.requirement.disposition, Disposition::Rejected);
                assert!(g.matching_versions_in_lock.is_empty());
            }
        }
    }
}

#[test]
fn external_links_identify_importers_and_snapshots_with_the_same_key() {
    let key = "app@1.0.0";
    let data = json!({"lockfileVersion":"9.0",
        "importers":{key:{"dependencies":{"other":{"specifier":"link:../shared","version":"link:../shared"}}}},
        "packages":{key:{"resolution":{}}},
        "snapshots":{key:{"dependencies":{"other":"link:shared"}}}});
    let report = inspect(&lock(&data), &PeerRules::default(), None).unwrap();
    let kinds: BTreeSet<_> = report
        .external_links
        .iter()
        .filter(|l| l.from == key && l.dependency == "other" && l.target == "shared")
        .map(|l| l.source_kind)
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from([
            locksmith::LinkSource::Importer,
            locksmith::LinkSource::Snapshot
        ])
    );
}
