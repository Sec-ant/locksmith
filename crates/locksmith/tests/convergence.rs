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
    let output = converge(
        &base,
        &donor,
        &[change("app", &before, &after)],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
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
fn adding_a_consumer_preserves_unaffected_peer_bindings_below_a_shared_release() {
    let parent = json!({"dependencies":{"parent":{"specifier":"1.0.0","version":"1.0.0"}}});
    let make = |installed: bool| {
        let mut snapshots = json!({
            "parent@1.0.0":{"dependencies":{"adapter":"1.0.0"}},
            "adapter@1.0.0":{}
        });
        if installed {
            snapshots["parent@1.0.0"]["dependencies"]["adapter"] = "1.0.0(extra@1.0.0)".into();
            snapshots["adapter@1.0.0(extra@1.0.0)"] =
                json!({"optionalDependencies":{"extra":"1.0.0"}});
            snapshots["extra@1.0.0"] = json!({});
        }
        let added = if installed { parent.clone() } else { json!({}) };
        let mut doc = lock(json!({"stable":parent,"added":added}), snapshots)
            .project()
            .unwrap()
            .clone();
        doc["packages"]["adapter@1.0.0"]["peerDependencies"] = json!({"extra":"^1"});
        doc["packages"]["adapter@1.0.0"]["peerDependenciesMeta"] =
            json!({"extra":{"optional":true}});
        parse(doc)
    };
    let result = converge(
        &make(false),
        &make(true),
        &[change(
            "added",
            &json!({}),
            &json!({"dependencies":{"parent":"1.0.0"}}),
        )],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
    result.validate().unwrap();
    let doc = result.project().unwrap();
    assert_eq!(doc["importers"]["added"], parent);
    assert_eq!(
        doc["snapshots"]["parent@1.0.0"]["dependencies"]["adapter"],
        "1.0.0"
    );
}

#[test]
fn each_importer_changes_only_the_peer_it_provides() {
    let adapter = |host: &str, extra: bool| {
        format!(
            "1.0.0{}(host@{host})",
            if extra { "(extra@3.0.0)" } else { "" }
        )
    };
    let pin =
        |specifier: &str, version: &str| json!({ "specifier": specifier, "version": version });
    let make = |installed: bool| {
        let old = adapter("1.0.0", installed);
        let new = adapter(if installed { "2.0.0" } else { "1.0.0" }, installed);
        let importers = json!({
            "stable": {"dependencies": {
                "edited": pin("workspace:*", "link:../edited"),
                "host": pin("1.0.0","1.0.0"),
                "adapter": pin("1.0.0",&old)
            }},
            "edited": {"dependencies": {
                "host": if installed {pin("2.0.0","2.0.0")} else {pin("1.0.0","1.0.0")},
                "adapter": pin("1.0.0",&new)
            }}
        });
        let mut snapshots = json!({"host@1.0.0":{}});
        if installed {
            snapshots["host@2.0.0"] = json!({});
            snapshots["extra@3.0.0"] = json!({});
        }
        for (key, host) in [
            (old, "1.0.0"),
            (new, if installed { "2.0.0" } else { "1.0.0" }),
        ] {
            let mut record = json!({"dependencies":{"host":host}});
            if installed {
                record["optionalDependencies"] = json!({"extra":"3.0.0"});
            }
            snapshots[format!("adapter@{key}")] = record;
        }
        let lock = lock(importers, snapshots);
        let mut doc = lock.project().unwrap().clone();
        doc["packages"]["adapter@1.0.0"]["peerDependencies"] = json!({"host":"*", "extra":"^3"});
        doc["packages"]["adapter@1.0.0"]["peerDependenciesMeta"] =
            json!({"extra":{"optional":true}});
        parse(doc)
    };
    let base = make(false);
    let updated = make(true);
    let before = json!({"name":"edited","dependencies":{"host":"1.0.0","adapter":"1.0.0"}});
    let after = json!({"name":"edited","dependencies":{"host":"2.0.0","adapter":"1.0.0"}});
    let out = converge(
        &base,
        &updated,
        &[change("edited", &before, &after)],
        Default::default(),
    )
    .unwrap();
    out.validate().unwrap();
    let doc = out.project().unwrap();
    assert_eq!(
        doc["importers"]["stable"],
        base.project().unwrap()["importers"]["stable"]
    );
    assert_eq!(
        doc["importers"]["edited"]["dependencies"]["adapter"]["version"],
        adapter("2.0.0", false)
    );
    assert_eq!(
        doc["snapshots"][format!("adapter@{}", adapter("2.0.0", false))],
        json!({"dependencies":{"host":"2.0.0"}})
    );
    assert!(doc["packages"].get("extra@3.0.0").is_none());
}

#[test]
fn shared_release_retains_its_locked_child_with_a_transitive_peer() {
    let dependency = json!({"helper":{"specifier":"1.0.0","version":"1.0.0(patch_hash=abc)"}});
    let make = |walker: &str, installed: bool| {
        let input = lock(
            json!({
                "stable":{"dependencies":dependency},
                "edited":if installed { json!({"dependencies":dependency}) } else {json!({})}
            }),
            json!({
                "helper@1.0.0(patch_hash=abc)":{"dependencies":{"walker":walker}},
                format!("walker@{walker}"):{"dependencies":{"debug":"1.0.0"},"transitivePeerDependencies":["color"]},
                "debug@1.0.0":{}
            }),
        );
        let mut doc = input.project().unwrap().clone();
        doc["patchedDependencies"] = json!({"helper@1.0.0":{"hash":"abc","path":"helper.patch"}});
        parse(doc)
    };
    let base = make("1.2.0", false);
    let installed = make("1.1.0", true);
    let result = converge(
        &base,
        &installed,
        &[change(
            "edited",
            &json!({}),
            &json!({"dependencies":{"helper":"1.0.0"}}),
        )],
        Default::default(),
    )
    .unwrap();
    result.validate().unwrap();
    assert_eq!(
        result.project().unwrap()["snapshots"],
        base.project().unwrap()["snapshots"]
    );
    assert_eq!(
        result.project().unwrap()["importers"]["edited"],
        json!({"dependencies":dependency})
    );
}

#[test]
fn shared_dependency_is_required_if_any_final_path_requires_it() {
    let leaf = json!({"leaf":{"specifier":"1.0.0","version":"1.0.0"}});
    let optional = json!({"optionalDependencies":leaf});
    let make = |required: bool| {
        lock(
            json!({
                "one":optional,
                "two":if required { json!({"dependencies":leaf}) } else { json!({}) }
            }),
            json!({"leaf@1.0.0":if required { json!({}) } else { json!({"optional":true}) }}),
        )
    };
    let empty = json!({});
    let required = json!({"dependencies":{"leaf":"1.0.0"}});
    for (before, after, add) in [(&empty, &required, true), (&required, &empty, false)] {
        let installed = make(add);
        let out = converge(
            &make(!add),
            &installed,
            &[change("two", before, after)],
            Default::default(),
        )
        .unwrap();
        assert_eq!(out.project(), installed.project());
    }
}

#[test]
fn adding_or_removing_a_root_provider_respects_workspace_inheritance() {
    let member = |version: &str| {
        json!({"dependencies":{
            "adapter":{"specifier":"1.0.0","version":version}
        }})
    };
    let make = |has_host: bool, inherits: bool| {
        let host = json!({"host":{"specifier":"2.0.0","version":"2.0.0"}});
        let bound = "1.0.0(host@2.0.0)";
        let mut snapshots = json!({"adapter@1.0.0":{}});
        if has_host {
            snapshots["host@2.0.0"] = json!({});
        }
        if has_host && inherits {
            snapshots = json!({
                "host@2.0.0":{},
                format!("adapter@{bound}"):{"optionalDependencies":{"host":"2.0.0"}}
            });
        }
        let input = lock(
            json!({
                ".":if has_host { json!({"dependencies":host}) } else { json!({}) },
                "app":member(if has_host && inherits { bound } else { "1.0.0" })
            }),
            snapshots,
        );
        let mut doc = input.project().unwrap().clone();
        doc["packages"]["adapter@1.0.0"]["peerDependencies"] = json!({"host":"^2"});
        doc["packages"]["adapter@1.0.0"]["peerDependenciesMeta"] =
            json!({"host":{"optional":true}});
        parse(doc)
    };
    let empty = json!({});
    let root = json!({"dependencies":{"host":"2.0.0"}});
    for inherits in [true, false] {
        let mut config = locksmith::WorkspaceConfig::default();
        config.resolution.workspace_root_peers = inherits;
        for (before, after, add) in [(&empty, &root, true), (&root, &empty, false)] {
            let installed = make(add, inherits);
            let result = converge(
                &make(!add, inherits),
                &installed,
                &[change(".", before, after)],
                locksmith::ConvergeOptions {
                    configuration: Some(&config),
                    registry: None,
                },
            )
            .unwrap();
            assert_eq!(result.project(), installed.project());
        }
    }
}
#[test]
fn stale_and_missing_candidates_are_errors() {
    let (base, _, before, after) = fixture();
    for donor in [&base, &lock(json!({}), json!({}))] {
        assert!(matches!(
            converge(
                &base,
                donor,
                &[change("app", &before, &after)],
                locksmith::ConvergeOptions::default()
            ),
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
        converge(
            &base,
            &parse(bad),
            &[change("app", &before, &after)],
            locksmith::ConvergeOptions::default()
        ),
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
        converge(
            &empty,
            &added,
            &[add],
            locksmith::ConvergeOptions::default()
        )
        .unwrap()
        .project(),
        added.project()
    );
    let delete = ManifestChange {
        importer: "app",
        before: Some(&manifest),
        after: None,
    };
    assert_eq!(
        converge(
            &added,
            &empty,
            &[delete],
            locksmith::ConvergeOptions::default()
        )
        .unwrap()
        .project(),
        empty.project()
    );
    assert!(matches!(
        converge(
            &added,
            &added,
            &[delete],
            locksmith::ConvergeOptions::default()
        ),
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
        locksmith::ConvergeOptions::default(),
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
        locksmith::ConvergeOptions::default(),
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
        locksmith::ConvergeOptions::default(),
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
        json!({"foo@3.0.0":{"optional":true}}),
    );
    let output = converge(
        &base,
        &donor,
        &[change("app", &before, &after)],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
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
        converge(
            &base,
            &donor,
            &[change("app", &before, &after)],
            locksmith::ConvergeOptions::default()
        )
        .unwrap()
        .project(),
        donor.project()
    );
    let mut bad = donor.project().unwrap().clone();
    bad["importers"]["app"]["dependencies"]["alias"]["version"] = json!("@scope/real@4.0.0");
    assert!(matches!(
        converge(
            &base,
            &parse(bad),
            &[change("app", &before, &after)],
            locksmith::ConvergeOptions::default()
        ),
        Err(LockfileError::ManifestMismatch { .. })
    ));
}

#[test]
fn unchanged_local_manifest_provides_auto_peer_context() {
    let make = |version: &str| {
        lock(
            json!({
                "app":{"dependencies":{"lib":{"specifier":"workspace:*","version":"link:../lib"},"foo":{"specifier":version,"version":version}}},
                "lib":{"dependencies":{"foo":{"specifier":"*","version":version}}},
                "consumer":{"dependencies":{"app":{"specifier":"workspace:*","version":"link:../app"},"foo":{"specifier":"*","version":version}}}
            }),
            json!({format!("foo@{version}"):{}}),
        )
    };
    let before = json!({"dependencies":{"lib":"workspace:*","foo":"1.0.0"}});
    let after = json!({"dependencies":{"lib":"workspace:*","foo":"2.0.0"}});
    let library = json!({"name":"lib","peerDependencies":{"foo":"*"}});
    let consumer = json!({"dependencies":{"app":"workspace:*"},"peerDependencies":{"foo":"*"}});
    let base = make("1.0.0");
    let candidate = make("2.0.0");
    assert!(
        matches!(converge(&base,&candidate,&[change("app",&before,&after)], locksmith::ConvergeOptions::default()),Err(LockfileError::ManifestMismatch{importer,..}) if importer=="lib")
    );
    let output = converge(
        &base,
        &candidate,
        &[
            change("app", &before, &after),
            change("lib", &library, &library),
            change("consumer", &consumer, &consumer),
        ],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["lib"]["dependencies"]["foo"]["version"],
        "2.0.0"
    );
    assert_eq!(
        output.project().unwrap()["importers"]["consumer"],
        base.project().unwrap()["importers"]["consumer"]
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
    let output = converge(
        &base,
        &make("1.1.0"),
        &[change("app", &before, &after)],
        locksmith::ConvergeOptions::default(),
    )
    .unwrap();
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
        converge(
            &make(false),
            &make(true),
            &[change("app", &before, &after)],
            locksmith::ConvergeOptions::default()
        )
        .unwrap()
        .project(),
        make(true).project()
    );
}

#[test]
fn edited_range_preserves_compatible_baseline_and_its_peer_consumer() {
    let make = |range: &str, host: &str, add: bool| {
        let widget = format!("1.0.0(host@{host})");
        let mut doc = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{
            "host":{"specifier":range,"version":host},"widget":{"specifier":"1.0.0","version":widget}
        }}},"packages":{format!("host@{host}"):{"resolution":{"integrity":"sha512-host"}},"widget@1.0.0":{"resolution":{"integrity":"sha512-widget"},"peerDependencies":{"host":"*"}}},
        "snapshots":{format!("host@{host}"): {}, format!("widget@{widget}"):{"dependencies":{"host":host}}}});
        if add {
            doc["importers"]["."]["dependencies"]["extra"] =
                json!({"specifier":"2.0.0","version":"2.0.0"});
            doc["packages"]["extra@2.0.0"] = json!({"resolution":{"integrity":"sha512-extra"}});
            doc["snapshots"]["extra@2.0.0"] = json!({});
        }
        parse(doc)
    };
    let before = json!({"dependencies":{"host":"^1","widget":"1.0.0"}});
    let base = make("^1", "1.0.0", false);
    for mixed in [false, true] {
        let mut after = json!({"dependencies":{"host":">=1 <2","widget":"1.0.0"}});
        if mixed {
            after["dependencies"]["extra"] = "2.0.0".into();
        }
        let candidate = make(">=1 <2", "1.1.0", mixed);
        let result = converge(
            &base,
            &candidate,
            &[change(".", &before, &after)],
            locksmith::ConvergeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.project(), make(">=1 <2", "1.0.0", mixed).project());
    }
}

#[test]
fn moving_a_compatible_dependency_keeps_the_pin_and_updates_optionality() {
    let base = lock(
        json!({"app":{"dependencies":{"foo":{"specifier":"^1","version":"1.0.0"}}}}),
        json!({"foo@1.0.0":{},"orphan@1.0.0":{}}),
    );
    let before = json!({"dependencies":{"foo":"^1"}});
    for group in ["devDependencies", "optionalDependencies"] {
        let after = json!({group:{"foo":"^1"}});
        let donor = lock(
            json!({"app":{group:{"foo":{"specifier":"^1","version":"1.1.0"}}}}),
            json!({"foo@1.1.0":{},"orphan@1.0.0":{}}),
        );
        let out = converge(
            &base,
            &donor,
            &[change("app", &before, &after)],
            Default::default(),
        )
        .unwrap();
        let doc = out.project().unwrap();
        assert_eq!(doc["importers"]["app"][group]["foo"]["version"], "1.0.0");
        assert!(doc["importers"]["app"].get("dependencies").is_none());
        assert_eq!(
            doc["snapshots"]["foo@1.0.0"]["optional"].as_bool(),
            (group == "optionalDependencies").then_some(true)
        );
        assert_eq!(doc["snapshots"]["orphan@1.0.0"], json!({}));
    }
}
