use locksmith::{Lockfile, LockfileError, ManifestChange, converge};
use serde_json::{Value, json};

fn lock(importers: Value, snapshots: Value) -> Lockfile {
    let mut packages = json!({});
    for key in snapshots.as_object().unwrap().keys() {
        packages[key.split('(').next().unwrap()] =
            json!({"resolution":{"integrity":"sha512-test"}});
    }
    parse(
        json!({"lockfileVersion":"9.0","importers":importers,"packages":packages,"snapshots":snapshots}),
    )
}
fn parse(value: Value) -> Lockfile {
    Lockfile::parse(&value.to_string()).unwrap()
}
fn change<'a>(id: &'a str, before: &'a Value, after: &'a Value) -> ManifestChange<'a> {
    ManifestChange {
        importer: id,
        before: Some(before),
        after: Some(after),
    }
}
fn fixture() -> (Lockfile, Lockfile, Value, Value) {
    let before = json!({"dependencies":{"foo":"1.0.0","bar":"^1.0.0"}});
    let after = json!({"dependencies":{"foo":"2.0.0","bar":"^1.0.0"}});
    let make = |primary: &str, sibling: &str| {
        lock(
            json!({"app":{"dependencies":{"foo":{"specifier":primary,"version":primary},"bar":{"specifier":"^1.0.0","version":sibling}}}}),
            json!({format!("foo@{primary}"):{"dependencies":{"leaf":primary}},format!("leaf@{primary}"):{},format!("bar@{sibling}"):{}}),
        )
    };
    (
        make("1.0.0", "1.0.0"),
        make("2.0.0", "1.1.0"),
        before,
        after,
    )
}
#[test]
fn changed_dependency_adopts_its_closure_and_retains_unchanged_sibling() {
    let (base, donor, before, after) = fixture();
    let output = converge(&base, &donor, &[change("app", &before, &after)]).unwrap();
    let p = output.project().unwrap();
    assert_eq!(
        p["importers"]["app"]["dependencies"]["bar"]["version"],
        "1.0.0"
    );
    assert!(p["snapshots"].get("leaf@2.0.0").is_some());
    assert!(p["snapshots"].get("leaf@1.0.0").is_none());
    assert!(p["snapshots"].get("bar@1.1.0").is_none());
    Lockfile::parse(&output.to_yaml().unwrap())
        .unwrap()
        .validate()
        .unwrap();
}
#[test]
fn stale_and_missing_candidates_are_errors() {
    let (base, _, before, after) = fixture();
    for donor in [&base, &lock(json!({}), json!({}))] {
        assert!(matches!(
            converge(&base, donor, &[change("app", &before, &after)]),
            Err(LockfileError::ManifestMismatch {
                side: "candidate",
                ..
            })
        ));
    }
}
#[test]
fn declared_ranges_and_resolved_versions_must_agree() {
    let (base, donor, before, after) = fixture();
    let mut bad = donor.project().unwrap().clone();
    bad["importers"]["app"]["dependencies"]["foo"]["version"] = json!("1.0.0");
    assert!(matches!(
        converge(&base, &parse(bad), &[change("app", &before, &after)]),
        Err(LockfileError::ManifestMismatch {
            side: "candidate",
            ..
        })
    ));
}
#[test]
fn additions_and_deletions_follow_the_manifests() {
    let empty = lock(json!({}), json!({}));
    let manifest = json!({});
    let added = lock(json!({"app":{}}), json!({}));
    let add = ManifestChange {
        importer: "app",
        before: None,
        after: Some(&manifest),
    };
    assert_eq!(
        converge(&empty, &added, &[add]).unwrap().project(),
        added.project()
    );
    let delete = ManifestChange {
        importer: "app",
        before: Some(&manifest),
        after: None,
    };
    assert_eq!(
        converge(&added, &empty, &[delete]).unwrap().project(),
        empty.project()
    );
    assert!(matches!(
        converge(&added, &added, &[delete]),
        Err(LockfileError::ManifestMismatch { .. })
    ));
}
#[test]
fn workspace_consumers_follow_changed_library_without_sibling_drift() {
    let make = |version: &str, sibling: &str| {
        lock(
            json!({
                "app":{"dependencies":{"lib":{"specifier":"workspace:*","version":"link:../lib"},"bar":{"specifier":"^1.0.0","version":sibling}}},
                "lib":{"dependencies":{"foo":{"specifier":version,"version":version}}}
            }),
            json!({format!("foo@{version}"):{},format!("bar@{sibling}"):{}}),
        )
    };
    let before = json!({"name":"lib","dependencies":{"foo":"1.0.0"}});
    let after = json!({"name":"lib","dependencies":{"foo":"2.0.0"}});
    let output = converge(
        &make("1.0.0", "1.0.0"),
        &make("2.0.0", "1.1.0"),
        &[change("lib", &before, &after)],
    )
    .unwrap();
    output.validate().unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["app"]["dependencies"]["bar"]["version"],
        "1.0.0"
    );
    assert!(
        output.project().unwrap()["snapshots"]
            .get("foo@2.0.0")
            .is_some()
    );
}
#[test]
fn file_workspace_consumers_use_the_new_local_package_snapshot() {
    let make = |version: &str| {
        let l = lock(
            json!({"app":{"dependencies":{"lib":{"specifier":"file:../lib","version":"file:lib"}}},"lib":{"dependencies":{"foo":{"specifier":version,"version":version}}}}),
            json!({"lib@file:lib":{"dependencies":{"foo":version}},format!("foo@{version}"):{}}),
        );
        let mut p = l.project().unwrap().clone();
        p["packages"]["lib@file:lib"]["resolution"] = json!({"type":"directory","directory":"lib"});
        parse(p)
    };
    let before = json!({"name":"lib","dependencies":{"foo":"1.0.0"}});
    let after = json!({"name":"lib","dependencies":{"foo":"2.0.0"}});
    let output = converge(
        &make("1.0.0"),
        &make("2.0.0"),
        &[change("lib", &before, &after)],
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["snapshots"]["lib@file:lib"]["dependencies"]["foo"],
        "2.0.0"
    );
    output.validate().unwrap();
}
#[test]
fn peer_provider_change_updates_transitive_peer_instances() {
    let make = |version: &str| {
        let plugin = format!("plugin@1.0.0(foo@{version})");
        let wrapper = format!("wrapper@1.0.0(foo@{version})");
        let l = lock(
            json!({"app":{"dependencies":{"foo":{"specifier":version,"version":version},"wrapper":{"specifier":"1.0.0","version":format!("1.0.0(foo@{version})")}}}}),
            json!({format!("foo@{version}"):{},plugin:{"dependencies":{"foo":version}},wrapper:{"dependencies":{"plugin":format!("1.0.0(foo@{version})")}}}),
        );
        let mut p = l.project().unwrap().clone();
        p["packages"]["plugin@1.0.0"]["peerDependencies"] = json!({"foo":"*"});
        parse(p)
    };
    let before = json!({"dependencies":{"foo":"1.0.0","wrapper":"1.0.0"}});
    let after = json!({"dependencies":{"foo":"2.0.0","wrapper":"1.0.0"}});
    let output = converge(
        &make("1.0.0"),
        &make("2.0.0"),
        &[change("app", &before, &after)],
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["app"]["dependencies"]["wrapper"]["version"],
        "1.0.0(foo@2.0.0)"
    );
    assert!(
        output.project().unwrap()["snapshots"]
            .get("foo@1.0.0")
            .is_none()
    );
}
#[test]
fn optional_and_production_precedence_and_group_moves_match_pnpm() {
    let before = json!({"devDependencies":{"foo":"1.0.0"}});
    let after = json!({"devDependencies":{"foo":"1.0.0"},"dependencies":{"foo":"2.0.0"},"optionalDependencies":{"foo":"3.0.0"}});
    let base = lock(
        json!({"app":{"devDependencies":{"foo":{"specifier":"1.0.0","version":"1.0.0"}}}}),
        json!({"foo@1.0.0":{}}),
    );
    let donor = lock(
        json!({"app":{"optionalDependencies":{"foo":{"specifier":"3.0.0","version":"3.0.0"}}}}),
        json!({"foo@3.0.0":{}}),
    );
    let output = converge(&base, &donor, &[change("app", &before, &after)]).unwrap();
    assert_eq!(output.project(), donor.project());
}
#[test]
fn auto_installed_peers_and_npm_alias_ranges_are_checked() {
    let before = json!({});
    let after =
        json!({"peerDependencies":{"foo":"^2"},"dependencies":{"alias":"npm:@scope/real@^3"}});
    let base = lock(json!({"app":{}}), json!({}));
    let donor = lock(
        json!({"app":{"dependencies":{"foo":{"specifier":"^2","version":"2.1.0"},"alias":{"specifier":"npm:@scope/real@^3","version":"@scope/real@3.2.0"}}}}),
        json!({"foo@2.1.0":{},"@scope/real@3.2.0":{}}),
    );
    assert_eq!(
        converge(&base, &donor, &[change("app", &before, &after)])
            .unwrap()
            .project(),
        donor.project()
    );
    let mut bad = donor.project().unwrap().clone();
    bad["importers"]["app"]["dependencies"]["alias"]["version"] = json!("@scope/real@4.0.0");
    assert!(matches!(
        converge(&base, &parse(bad), &[change("app", &before, &after)]),
        Err(LockfileError::ManifestMismatch { .. })
    ));
}

#[test]
fn unchanged_local_manifest_provides_auto_peer_context() {
    let make = |version: &str| {
        lock(
            json!({
                "app":{"dependencies":{"lib":{"specifier":"workspace:*","version":"link:../lib"},"foo":{"specifier":version,"version":version}}},
                "lib":{"dependencies":{"foo":{"specifier":"*","version":version}}}
            }),
            json!({format!("foo@{version}"):{}}),
        )
    };
    let before = json!({"dependencies":{"lib":"workspace:*","foo":"1.0.0"}});
    let after = json!({"dependencies":{"lib":"workspace:*","foo":"2.0.0"}});
    let library = json!({"name":"lib","peerDependencies":{"foo":"*"}});
    let base = make("1.0.0");
    let candidate = make("2.0.0");
    assert!(
        matches!(converge(&base,&candidate,&[change("app",&before,&after)]),Err(LockfileError::ManifestMismatch{importer,..}) if importer=="lib")
    );
    let output = converge(
        &base,
        &candidate,
        &[
            change("app", &before, &after),
            change("lib", &library, &library),
        ],
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["lib"]["dependencies"]["foo"]["version"],
        "2.0.0"
    );
}

#[test]
fn package_version_change_does_not_upgrade_unchanged_dependencies() {
    let before = json!({"name":"app","version":"1.0.0","dependencies":{"bar":"^1.0.0"}});
    let after = json!({"name":"app","version":"2.0.0","dependencies":{"bar":"^1.0.0"}});
    let make = |version: &str| {
        lock(
            json!({"app":{"dependencies":{"bar":{"specifier":"^1.0.0","version":version}}}}),
            json!({format!("bar@{version}"):{}}),
        )
    };
    let base = make("1.0.0");
    let output = converge(&base, &make("1.1.0"), &[change("app", &before, &after)]).unwrap();
    assert_eq!(output.project(), base.project());
}

#[test]
fn overrides_apply_before_manifest_comparison_including_removal_and_parent_rules() {
    let before = json!({"name":"app","version":"1.0.0","dependencies":{"foo":"^1","gone":"1.0.0"}});
    let after = json!({"name":"app","version":"1.0.0","dependencies":{"foo":"^1","gone":"1.0.0","bar":"1.0.0"}});
    let make = |added: bool| {
        let mut deps = json!({"foo":{"specifier":"3.0.0","version":"3.0.0"}});
        let mut snapshots = json!({"foo@3.0.0":{}});
        if added {
            deps["bar"] = json!({"specifier":"1.0.0","version":"1.0.0"});
            snapshots["bar@1.0.0"] = json!({});
        }
        let l = lock(json!({"app":{"dependencies":deps}}), snapshots);
        let mut p = l.project().unwrap().clone();
        p["overrides"] = json!({"foo":"2.0.0","app@^1>foo@^1":"3.0.0","gone":"-"});
        parse(p)
    };
    assert_eq!(
        converge(&make(false), &make(true), &[change("app", &before, &after)])
            .unwrap()
            .project(),
        make(true).project()
    );
}
