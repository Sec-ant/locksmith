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
fn with_times(lock: &Lockfile, times: Value) -> Lockfile {
    let mut doc = lock.project().unwrap().clone();
    doc["time"] = times;
    parse(doc)
}
fn change<'a>(id: &'a str, before: &'a Value, after: &'a Value) -> ManifestChange<'a> {
    ManifestChange {
        importer: id,
        before: Some(before),
        after: Some(after),
    }
}

#[test]
fn convergence_rejects_a_reachable_snapshot_with_an_undeclared_patch() {
    let base = lock(json!({".":{}}), json!({}));
    let candidate = lock(
        json!({".":{"dependencies":{"foo":{"specifier":"1.0.0","version":"1.0.0(patch_hash=h)"}}}}),
        json!({"foo@1.0.0(patch_hash=h)":{}}),
    );
    let result = converge(
        &base,
        &candidate,
        &[change(
            ".",
            &json!({}),
            &json!({"dependencies":{"foo":"1.0.0"}}),
        )],
        Default::default(),
    );
    assert!(matches!(result, Err(LockfileError::InvalidGraph(issues))
        if issues.iter().any(|issue| issue.path == ["snapshots", "foo@1.0.0(patch_hash=h)"])));
}

#[test]
fn unchanged_pnpm_configuration_can_be_supplied_once() {
    let input = lock(json!({".": {}}), json!({}));
    let before = json!({"packageManager":"pnpm@12.21.0"});
    let after =
        json!({"packageManager":"pnpm@12.21.0","pnpm":{"ignoredBuiltDependencies":["tool"]}});
    let config = locksmith::WorkspaceConfig::from_pnpm_version(
        &json!({"pmOnFail":"ignore"}),
        &"12.21.0".parse().unwrap(),
    )
    .unwrap();
    for baseline_configuration in [None, Some(&config)] {
        let output = converge(
            &input,
            &input,
            &[change(".", &before, &after)],
            locksmith::ConvergeOptions {
                configuration: Some(&config),
                baseline_configuration,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output.to_yaml().unwrap(), input.to_yaml().unwrap());
    }
    let legacy =
        locksmith::WorkspaceConfig::from_pnpm_version(&json!({}), &"10.34.9".parse().unwrap())
            .unwrap();
    let result = converge(
        &input,
        &input,
        &[change(
            ".",
            &json!({}),
            &json!({"pnpm":{"overrides":{"lib":"1"}}}),
        )],
        locksmith::ConvergeOptions {
            configuration: Some(&legacy),
            ..Default::default()
        },
    );
    assert!(
        matches!(result, Err(LockfileError::GlobalConfigurationChanged{field,..}) if field=="pnpm")
    );
}

#[test]
fn recorded_and_authored_baselines_validate_their_own_specifiers() {
    let input = lock(
        json!({"app":{"dependencies":{"lib":{"specifier":"2.0.0","version":"2.0.0"}}}}),
        json!({"lib@2.0.0":{}}),
    );
    let mut doc = input.project().unwrap().clone();
    doc["overrides"] = json!({"lib@1.0.0":"2.0.0","lib@2.0.0":"3.0.0"});
    let lock = parse(doc);
    let authored = json!({"dependencies":{"lib":"1.0.0"}});
    let recorded = json!({"dependencies":{"lib":"2.0.0"}});
    let reconstructed = std::collections::BTreeSet::from(["app".to_owned()]);
    let check = |before: &Value, after: &Value| {
        locksmith::converge(
            &lock,
            &lock,
            &[change("app", before, after)],
            locksmith::ConvergeOptions {
                recorded_baselines: Some(&reconstructed),
                ..Default::default()
            },
        )
    };
    assert!(check(&recorded, &authored).is_ok());
    for (before, after, side) in [
        (&authored, &authored, "baseline"),
        (&recorded, &recorded, "candidate"),
    ] {
        assert!(
            matches!(check(before, after), Err(LockfileError::ManifestMismatch {side: actual,..}) if actual == side)
        );
    }
    assert!(matches!(
        converge(
            &lock,
            &lock,
            &[change("app", &recorded, &authored)],
            Default::default()
        ),
        Err(LockfileError::ManifestMismatch {
            side: "baseline",
            ..
        })
    ));
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
fn unchanged_manifests_do_not_restore_a_broken_baseline_graph() {
    let manifests = json!({"dependencies":{"lib":"^1.0.0"}});
    let importers = json!({".":{"dependencies":{"lib":{"specifier":"^1.0.0","version":"1.0.0"}}}});
    let baseline = lock(
        importers.clone(),
        json!({"lib@1.0.0":{"dependencies":{"leaf":"1.0.0"}}}),
    );
    let candidate = lock(
        importers,
        json!({"lib@1.0.0":{"dependencies":{"leaf":"1.0.0"}},"leaf@1.0.0":{}}),
    );
    assert!(matches!(
        converge(
            &baseline,
            &candidate,
            &[change(".", &manifests, &manifests)],
            Default::default()
        ),
        Err(LockfileError::InvalidGraph(_))
    ));
}

#[test]
fn converge_rejects_a_stale_baseline_even_when_the_candidate_settings_match() {
    let mut doc = json!({"lockfileVersion":"9.0","importers":{".":{}}});
    let baseline = parse(doc.clone());
    let settings = json!({"addMissingPeerTypes":true});
    let config =
        locksmith::WorkspaceConfig::from_pnpm_version(&settings, &"12.21.0".parse().unwrap())
            .unwrap();
    doc["settings"] = settings;
    let candidate = parse(doc);
    let before = json!({});
    let after = json!({"dependencies":{}});
    let declarations = [change(".", &before, &after)];
    for changes in [&[][..], &declarations[..]] {
        let result = converge(
            &baseline,
            &candidate,
            changes,
            locksmith::ConvergeOptions {
                configuration: Some(&config),
                ..Default::default()
            },
        );
        assert!(
            matches!(result, Err(LockfileError::ResolutionRequired { path, .. })
            if path == ["settings", "addMissingPeerTypes"])
        );
    }
}

#[test]
fn publication_times_follow_the_direct_versions_selected_from_both_lockfiles() {
    let (baseline, candidate, before, after) = fixture();
    let old = "2020-01-01T00:00:00.000Z";
    let new = "2024-01-01T00:00:00.000Z";
    for (version, prefix) in [("9.14.4", "/"), ("12.21.0", "")] {
        let config =
            locksmith::WorkspaceConfig::from_pnpm_version(&json!({}), &version.parse().unwrap())
                .unwrap();
        let old_foo = format!("{prefix}foo@1.0.0");
        let new_foo = format!("{prefix}foo@2.0.0");
        let kept_bar = format!("{prefix}bar@1.0.0");
        let drifted_bar = format!("{prefix}bar@1.1.0");
        let changes = [change("app", &before, &after)];
        let result = converge(
            &with_times(&baseline, json!({&old_foo:old, &kept_bar:old})),
            &with_times(&candidate, json!({&new_foo:new, drifted_bar:new})),
            &changes,
            locksmith::ConvergeOptions {
                configuration: Some(&config),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            result.project().unwrap()["time"],
            json!({&kept_bar:old, &new_foo:new})
        );
    }
}

#[test]
fn a_dependency_that_becomes_transitive_loses_its_last_publication_time() {
    let (installed, _, after, _) = fixture();
    let mut before = after.clone();
    before["devDependencies"] = json!({"leaf":"1.0.0"});
    let mut base = installed.project().unwrap().clone();
    base["importers"]["app"]["devDependencies"] =
        json!({"leaf":{"specifier":"1.0.0","version":"1.0.0"}});
    base["time"] = json!({"/leaf@1.0.0":"2020-01-01T00:00:00.000Z"});
    let result = converge(
        &parse(base),
        &with_times(&installed, json!({})),
        &[change("app", &before, &after)],
        Default::default(),
    )
    .unwrap();
    assert_eq!(result.project().unwrap()["time"], json!({}));
}

#[test]
fn a_reinstall_that_only_adds_publication_times_keeps_the_original_lockfile() {
    let (baseline, _, _, _) = fixture();
    let candidate = with_times(&baseline, json!({"/foo@1.0.0":"2020-01-01T00:00:00.000Z"}));
    let result = converge(&baseline, &candidate, &[], Default::default()).unwrap();
    assert_eq!(result.to_yaml().unwrap(), baseline.to_yaml().unwrap());
}

#[test]
fn convergence_minimizes_provider_changes_across_two_consumers() {
    let versions = ["1.0.0", "2.0.0"];
    let assignments = versions.map(|a| versions.map(|b| [a, b])).concat();
    let apps = ["a", "b"];
    let manifest = |range: &str| json!({"dependencies":{"host":range, "widget":"1.0.0"}});
    let make = |ranges: [&str; 2], pins: [&str; 2]| {
        let mut importers = json!({});
        let mut snapshots = json!({});
        let mut packages = json!({"widget@1.0.0":{
            "resolution":{"integrity":"sha512-widget"}, "peerDependencies":{"host":"*"}
        }});
        for ((app, range), host) in apps.into_iter().zip(ranges).zip(pins) {
            let widget = format!("1.0.0(host@{host})");
            importers[app] = json!({"dependencies":{
                "host":{"specifier":range,"version":host}, "widget":{"specifier":"1.0.0","version":widget}
            }});
            packages[format!("host@{host}")] =
                json!({"resolution":{"integrity":format!("sha512-host-{host}")}});
            snapshots[format!("host@{host}")] = json!({});
            snapshots[format!("widget@{widget}")] = json!({"dependencies":{"host":host}});
        }
        parse(
            json!({"lockfileVersion":"9.0","importers":importers,"packages":packages,"snapshots":snapshots}),
        )
    };
    let before = manifest("*");
    for &original in &assignments {
        let base = make(["*"; 2], original);
        for a in ["*", versions[0], versions[1]] {
            for b in ["*", versions[0], versions[1]] {
                let ranges = [a, b];
                let allowed: Vec<_> = assignments
                    .iter()
                    .copied()
                    .filter(|pins| {
                        ranges
                            .iter()
                            .zip(pins)
                            .all(|(&range, &pin)| range == "*" || range == pin)
                    })
                    .collect();
                let edits = |pins: &[&str; 2]| {
                    pins.iter()
                        .zip(original)
                        .filter(|(new, old)| **new != *old)
                        .count()
                };
                let minimum = allowed.iter().map(edits).min().unwrap();
                let after = ranges.map(manifest);
                let changes = [
                    change("a", &before, &after[0]),
                    change("b", &before, &after[1]),
                ];
                for &installed in &allowed {
                    let donor = make(ranges, installed);
                    let result = converge(&base, &donor, &changes, Default::default()).unwrap();
                    let doc = result.project().unwrap();
                    let selected = apps.map(|app| {
                        let deps = &doc["importers"][app]["dependencies"];
                        let host = deps["host"]["version"].as_str().unwrap();
                        let widget = deps["widget"]["version"].as_str().unwrap();
                        assert_eq!(widget, format!("1.0.0(host@{host})"));
                        assert_eq!(
                            doc["snapshots"][format!("widget@{widget}")]["dependencies"]["host"],
                            host
                        );
                        host
                    });
                    assert!(allowed.contains(&selected));
                    assert_eq!(
                        edits(&selected),
                        minimum,
                        "{original:?} {ranges:?} {installed:?}"
                    );
                }
            }
        }
    }
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
fn convergence_preserves_the_original_yaml_for_an_untouched_package() {
    let (_, donor, before, after) = fixture();
    let original =
        "  bar@1.0.0:\n    resolution:\n      integrity: sha512-test # keep this annotation\n";
    let base = Lockfile::parse(&format!(
        "lockfileVersion: '9.0'\nimporters:\n  app:\n    dependencies:\n      foo: {{specifier: 1.0.0, version: 1.0.0}}\n      bar: {{specifier: ^1.0.0, version: 1.0.0}}\npackages:\n{original}  foo@1.0.0: {{resolution: {{integrity: sha512-test}}}}\nsnapshots:\n  bar@1.0.0: {{}}\n  foo@1.0.0: {{}}\n"
    )).unwrap();
    let result = converge(
        &base,
        &donor,
        &[change("app", &before, &after)],
        Default::default(),
    )
    .unwrap();
    assert!(result.to_yaml().unwrap().contains(original));
    assert_eq!(
        result.project().unwrap()["importers"]["app"]["dependencies"]["foo"]["version"],
        "2.0.0"
    );
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
                    baseline_configuration: None,
                    configuration: Some(&config),
                    ..Default::default()
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

#[test]
fn an_extension_preserves_siblings_and_stops_at_an_existing_peer_provider() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    use md5::{Digest, Md5};

    let spec = json!({"adapter@1.0.0":{"dependencies":{"renderer":"1.0.0"}}});
    // pnpm 9 writes the MD5 of the serialized extension rules.
    let checksum = format!("{:x}", Md5::digest(spec.to_string().as_bytes()));
    let before = WorkspaceConfig::default();
    let after = WorkspaceConfig::from_pnpm(&json!({"packageExtensions":spec})).unwrap();
    let options = ConvergeOptions {
        baseline_configuration: Some(&before),
        configuration: Some(&after),
        ..Default::default()
    };
    let make = |installed: bool| {
        let host = if installed {
            "1.0.0(theme@2.0.0)"
        } else {
            "1.0.0(theme@1.0.0)"
        };
        let theme = if installed { "2.0.0" } else { "1.0.0" };
        let adapter = if installed {
            "1.0.0(ui@1.0.0)"
        } else {
            "1.0.0"
        };
        let renderer = "1.0.0(ui@1.0.0)";
        let mut body = json!({"dependencies":{"theme":theme}});
        if installed {
            body["dependencies"]["renderer"] = renderer.into();
            body["transitivePeerDependencies"] = json!(["ui"]);
        }
        let lock = lock(
            json!({".":{"dependencies":{"host":{"specifier":"1.0.0","version":host}}}}),
            json!({
                format!("host@{host}"):{"dependencies":{"adapter":adapter,"renderer":renderer,"ui":"1.0.0","theme":theme}},
                format!("adapter@{adapter}"):body,
                format!("renderer@{renderer}"):{"dependencies":{"ui":"1.0.0"}},
                "ui@1.0.0":{},format!("theme@{theme}"): {}
            }),
        );
        let mut doc = lock.project().unwrap().clone();
        doc["packages"]["renderer@1.0.0"]["peerDependencies"] = json!({"ui":"1.0.0"});
        doc["packages"]["host@1.0.0"]["peerDependencies"] = json!({"theme":"*"});
        if installed {
            doc["packageExtensionsChecksum"] = checksum.clone().into();
        }
        parse(doc)
    };
    let base = make(false);
    let installed = make(true);
    let result = converge(&base, &installed, &[], options).unwrap();
    let doc = result.project().unwrap();
    assert_eq!(doc["importers"], base.project().unwrap()["importers"]);
    assert_eq!(doc["packages"], base.project().unwrap()["packages"]);
    assert_eq!(
        doc["snapshots"]["host@1.0.0(theme@1.0.0)"]["dependencies"],
        json!({"adapter":"1.0.0(ui@1.0.0)","renderer":"1.0.0(ui@1.0.0)","ui":"1.0.0","theme":"1.0.0"})
    );
    assert_eq!(
        doc["snapshots"]["adapter@1.0.0(ui@1.0.0)"]["dependencies"],
        json!({"theme":"1.0.0","renderer":"1.0.0(ui@1.0.0)"})
    );
    assert_eq!(
        doc["snapshots"].as_object().unwrap().len(),
        base.project().unwrap()["snapshots"]
            .as_object()
            .unwrap()
            .len()
    );

    let mut stale = base.project().unwrap().clone();
    assert!(converge(&base, &parse(stale.clone()), &[], options).is_err());
    stale["packageExtensionsChecksum"] = checksum.into();
    assert!(converge(&base, &parse(stale.clone()), &[], options).is_err());
    stale["snapshots"]["adapter@1.0.0"]["dependencies"]["renderer"] = "2.0.0".into();
    stale["snapshots"]["renderer@2.0.0"] = json!({});
    stale["packages"]["renderer@2.0.0"] = json!({"resolution":{"integrity":"sha512-test"}});
    assert!(converge(&base, &parse(stale), &[], options).is_err());
}

#[test]
fn editing_an_optional_extension_range_keeps_an_admissible_pin() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    use md5::{Digest, Md5};

    let make = |range: &str, pin: &str, catalog: &str| {
        let rules = json!({"host@1":{"optionalDependencies":{"extra":range}}});
        let checksum = format!("{:x}", Md5::digest(rules.to_string().as_bytes()));
        let config = WorkspaceConfig::from_pnpm(&json!({
            "packageExtensions":rules, "catalog":{"host":catalog}
        }))
        .unwrap();
        let mut doc = lock(
            json!({".":{"dependencies":{"host":{"specifier":"catalog:","version":"1.0.0"}}}}),
            json!({"host@1.0.0":{"optionalDependencies":{"extra":pin}}, format!("extra@{pin}"):{"optional":true}}),
        ).project().unwrap().clone();
        doc["packageExtensionsChecksum"] = checksum.into();
        (config, parse(doc))
    };
    let (before, base) = make("1.0.0", "1.0.0", "^1");
    let (after, installed) = make("^1", "1.1.0", "^1");
    let options = ConvergeOptions {
        baseline_configuration: Some(&before),
        configuration: Some(&after),
        ..Default::default()
    };
    let result = converge(&base, &installed, &[], options).unwrap();
    let doc = result.project().unwrap();
    assert_eq!(doc["snapshots"], base.project().unwrap()["snapshots"]);
    assert_eq!(doc["packages"], base.project().unwrap()["packages"]);
    assert_eq!(
        doc["packageExtensionsChecksum"],
        installed.project().unwrap()["packageExtensionsChecksum"]
    );
    assert!(doc.get("catalogs").is_none());

    let (before, base) = make("1.0.0", "1.0.0", "^2");
    let (after, installed) = make("^1", "1.1.0", "^2");
    let options = ConvergeOptions {
        baseline_configuration: Some(&before),
        configuration: Some(&after),
        ..Default::default()
    };
    assert!(converge(&base, &installed, &[], options).is_err());
}

#[test]
fn a_changed_local_provider_rebinds_its_sibling_and_keeps_the_siblings_other_dependencies() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    use md5::{Digest, Md5};
    let make = |provider: &str, leaf: &str| {
        let ext = json!({"host@1":{"dependencies":{"renderer":provider}}});
        let cfg = WorkspaceConfig::from_pnpm(&json!({"packageExtensions":ext})).unwrap();
        let plugin = format!("1.0.0(renderer@{provider})");
        let data = lock(
            json!({".":{"dependencies":{"host":{"specifier":"1.0.0","version":"1.0.0"}}}}),
            json!({"host@1.0.0":{"dependencies":{"renderer":provider,"plugin":plugin}},
                format!("plugin@{plugin}"):{"dependencies":{"renderer":provider,"leaf":leaf}},
                format!("renderer@{provider}"): {}, format!("leaf@{leaf}"): {}}),
        );
        let mut doc = data.project().unwrap().clone();
        doc["packages"]["plugin@1.0.0"]["peerDependencies"] = json!({"renderer":"^1"});
        doc["packageExtensionsChecksum"] = format!("{:x}", Md5::digest(ext.to_string())).into();
        (cfg, parse(doc))
    };
    let (old, base) = make("1.0.0", "1.0.0");
    let (new, fresh) = make("1.1.0", "1.2.0");
    let (_, expected) = make("1.1.0", "1.0.0");
    let opts = ConvergeOptions {
        baseline_configuration: Some(&old),
        configuration: Some(&new),
        ..Default::default()
    };
    let result = converge(&base, &fresh, &[], opts).unwrap();
    assert_eq!(result.project(), expected.project());
    assert_eq!(
        converge(&base, &result, &[], opts)
            .unwrap()
            .to_yaml()
            .unwrap(),
        result.to_yaml().unwrap()
    );
}

#[test]
fn a_new_consumer_can_reuse_an_exact_catalog_release_without_adding_a_catalog_record() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    let make = |installed: bool| {
        let spec = if installed {
            "catalog:toolchain"
        } else {
            "2.8.1"
        };
        let manifest = json!({"dependencies":{"tslib":spec}});
        let lock = lock(
            json!({"app":{"dependencies":{"tslib":{"specifier":spec,"version":"2.8.1"}}}}),
            json!({"tslib@2.8.1":{}}),
        );
        let mut doc = lock.project().unwrap().clone();
        let mut config = json!({"catalogs":{"toolchain":{"tslib":"2.8.1"}}});
        if installed {
            config["overrides"] = json!({"unused":"1.0.0"});
            doc["overrides"] = config["overrides"].clone();
            doc["catalogs"] =
                json!({"toolchain":{"tslib":{"specifier":"2.8.1","version":"2.8.1"}}});
        }
        (
            WorkspaceConfig::from_pnpm(&config).unwrap(),
            manifest,
            parse(doc),
        )
    };
    let (before, old, base) = make(false);
    let (after, new, installed) = make(true);
    let changes = [change("app", &old, &new)];
    let opts = ConvergeOptions {
        baseline_configuration: Some(&before),
        configuration: Some(&after),
        ..Default::default()
    };
    let result = converge(&base, &installed, &changes, opts).unwrap();
    let doc = result.project().unwrap();
    assert_eq!(doc["importers"], installed.project().unwrap()["importers"]);
    assert_eq!(doc["snapshots"], base.project().unwrap()["snapshots"]);
    assert!(doc.get("catalogs").is_none());
    assert_eq!(
        converge(&base, &result, &changes, opts)
            .unwrap()
            .to_yaml()
            .unwrap(),
        result.to_yaml().unwrap()
    );
}

#[test]
fn convergence_keeps_a_workspace_project_order_shared_by_both_locks() {
    let make = |foo: &str, stable: &str, extra: bool| {
        let mut importers = json!({"z":{"dependencies":{"foo":{"specifier":foo,"version":foo}}}});
        if extra {
            importers["m"] = json!({});
        }
        importers["a"] = json!({"dependencies":{"stable":{"specifier":"^1","version":stable}}});
        lock(
            importers,
            json!({format!("foo@{foo}"): {},format!("stable@{stable}"): {}}),
        )
    };
    let base = make("1.0.0", "1.0.0", false);
    let before = json!({"dependencies":{"foo":"1.0.0"}});
    let after = json!({"dependencies":{"foo":"2.0.0"}});
    let added = json!({"name":"m"});
    for (pin, extra) in [("1.0.0", false), ("1.1.0", false), ("1.1.0", true)] {
        let mut changes = vec![change("z", &before, &after)];
        if extra {
            changes.push(ManifestChange {
                importer: "m",
                before: None,
                after: Some(&added),
            });
        }
        let installed = make("2.0.0", pin, extra);
        let result = converge(&base, &installed, &changes, Default::default()).unwrap();
        let raw = result.to_yaml().unwrap();
        let parsed = Lockfile::parse(&raw).unwrap();
        let importers = parsed.project().unwrap()["importers"].as_object().unwrap();
        let expected = if extra {
            &["z", "m", "a"][..]
        } else {
            &["z", "a"][..]
        };
        assert!(
            importers
                .keys()
                .map(String::as_str)
                .eq(expected.iter().copied())
        );
        assert_eq!(importers["a"]["dependencies"]["stable"]["version"], "1.0.0");
        assert_eq!(
            converge(&base, &parsed, &changes, Default::default())
                .unwrap()
                .to_yaml()
                .unwrap(),
            raw
        );
    }
}

#[test]
fn a_shared_peer_context_can_keep_the_installed_assignment_and_restore_independent_pins() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    use md5::{Digest, Md5};

    let rules = json!({"adapter@1":{"dependencies":{"bridge":"1.0.0"}}});
    let before = WorkspaceConfig::default();
    let after = WorkspaceConfig::from_pnpm(&json!({"packageExtensions":rules})).unwrap();
    let options = ConvergeOptions {
        baseline_configuration: Some(&before),
        configuration: Some(&after),
        ..Default::default()
    };
    let make = |installed: bool, leaf: &str, palette: &str| {
        let mut roots = json!({});
        let mut states = json!({});
        for (app, host) in [
            ("a", if installed { "1.1.0" } else { "1.0.0" }),
            ("b", "2.0.0"),
        ] {
            let contextual = format!("1.0.0(host@{host})");
            let adapter = if installed {
                contextual.as_str()
            } else {
                "1.0.0"
            };
            roots[app] = json!({"dependencies":{
                "parent":{"specifier":"1.0.0","version":contextual},
                "host":{"specifier":"*","version":host},
                "stable":{"specifier":"^1","version":leaf},
                "utility":{"specifier":"1.0.0","version":"1.0.0"}
            }});
            states[format!("parent@{contextual}")] = json!({"dependencies":{"adapter":adapter,"plugin":contextual},"transitivePeerDependencies":["host"]});
            states[format!("plugin@{contextual}")] = json!({"dependencies":{"host":host}});
            states[format!("host@{host}")] = json!({});
            states[format!("adapter@{adapter}")] = if installed {
                states[format!("bridge@{contextual}")] = json!({"dependencies":{"host":host}});
                json!({"dependencies":{"bridge":contextual},"transitivePeerDependencies":["host"]})
            } else {
                json!({})
            };
        }
        states["utility@1.0.0"] = json!({"dependencies":{"child":leaf}});
        for n in ["child", "stable"] {
            states[format!("{n}@{leaf}")] = json!({});
        }
        let reporter = format!("1.0.0(palette@{palette})");
        roots["tools"] =
            json!({"dependencies":{"reporter":{"specifier":"1.0.0","version":reporter}}});
        states[format!("reporter@{reporter}")] =
            json!({"dependencies":{"palette":palette,"utility":"1.0.0"}});
        states[format!("palette@{palette}")] = json!({});
        states[format!("stable@{leaf}")] = json!({"dependencies":{"nested":leaf}});
        states[format!("nested@{leaf}")] = json!({});
        let mut doc = lock(roots, states).project().unwrap().clone();
        doc["packages"]["reporter@1.0.0"]["peerDependencies"] = json!({"palette":"^1"});
        doc["packages"]["plugin@1.0.0"]["peerDependencies"] = json!({"host":"*"});
        if installed {
            doc["packageExtensionsChecksum"] =
                format!("{:x}", Md5::digest(rules.to_string())).into();
            doc["packages"]["bridge@1.0.0"]["peerDependencies"] = json!({"host":"*"});
        }
        parse(doc)
    };
    let base = make(false, "1.0.0", "1.0.0");
    let installed = make(true, "1.1.0", "1.1.0");
    let result = converge(&base, &installed, &[], options).unwrap();
    assert_eq!(result.project(), make(true, "1.0.0", "1.0.0").project());
    assert_eq!(
        converge(&base, &result, &[], options)
            .unwrap()
            .to_yaml()
            .unwrap(),
        result.to_yaml().unwrap()
    );

    let mut invalid = installed.project().unwrap().clone();
    invalid["snapshots"]["adapter@1.0.0(host@2.0.0)"] =
        json!({"transitivePeerDependencies":["host"]});
    assert!(converge(&base, &parse(invalid), &[], options).is_err());
}

#[test]
fn a_catalog_consumer_keeps_its_compatible_pin_when_the_cached_release_changes() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};

    let manifest = json!({"dependencies":{"tool":"catalog:tools"}});
    let range = "npm:@scope/tool@^1.0.0";
    let config = json!({"catalogs":{"tools":{"tool":range}}});
    let before = WorkspaceConfig::from_pnpm(&config).unwrap();
    let mut changed = config;
    changed["catalog"] = json!({"unrelated":"2.0.0"});
    let after = WorkspaceConfig::from_pnpm(&changed).unwrap();
    let options = ConvergeOptions {
        baseline_configuration: Some(&before),
        configuration: Some(&after),
        ..Default::default()
    };
    let make = |cached: &str, imported: &str| {
        let source = lock(
            json!({
                "app":{"dependencies":{"tool":{"specifier":"catalog:tools","version":imported}}},
                "second":{"dependencies":{"tool":{"specifier":"catalog:tools","version":"@scope/tool@1.0.0"}}}
            }),
            json!({"@scope/tool@1.0.0":{},"@scope/tool@1.0.1":{},"@scope/tool@2.0.0":{},"other@1.0.1":{}}),
        );
        let mut doc = source.project().unwrap().clone();
        doc["catalogs"] = json!({"tools":{"tool":{"specifier":range,"version":cached}}});
        parse(doc)
    };
    let base = make("1.0.1", "@scope/tool@1.0.1");
    let installed = make("1.0.0", "@scope/tool@1.0.1");
    let changes = [
        change("app", &manifest, &manifest),
        change("second", &manifest, &manifest),
    ];
    let result = converge(&base, &installed, &changes, options).unwrap();
    assert_eq!(
        result.project().unwrap()["importers"],
        base.project().unwrap()["importers"]
    );
    assert_eq!(
        result.project().unwrap()["catalogs"],
        base.project().unwrap()["catalogs"]
    );
    installed.validate().unwrap();
    assert_eq!(
        converge(&base, &result, &changes, options)
            .unwrap()
            .to_yaml()
            .unwrap(),
        result.to_yaml().unwrap()
    );

    for (cached, imported) in [
        ("1.0.0", "@scope/tool@2.0.0"),
        ("1.0.0", "other@1.0.1"),
        ("2.0.0", "@scope/tool@1.0.1"),
    ] {
        let invalid = make(cached, imported);
        assert!(invalid.validate().is_err());
        assert!(converge(&base, &invalid, &changes, options).is_err());
    }
}

#[test]
fn an_exact_override_restores_the_required_extension_in_each_consumers_peer_scope() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    use md5::{Digest, Md5};

    let extension = json!({"host@1.0.0":{"dependencies":{"addon":"1.0.0"}}});
    let overrides = json!({"addon":"2.0.0"});
    let before = WorkspaceConfig::from_pnpm(&json!({"overrides":overrides})).unwrap();
    let after =
        WorkspaceConfig::from_pnpm(&json!({"overrides":overrides, "packageExtensions":extension}))
            .unwrap();
    let options = ConvergeOptions {
        baseline_configuration: Some(&before),
        configuration: Some(&after),
        ..Default::default()
    };
    let make = |installed: bool, second_witness: bool| {
        let mut importers = json!({});
        let mut snapshots = json!({});
        for (i, ui) in [("a", "1.0.0"), ("b", "2.0.0")] {
            let host = format!("1.0.0(ui@{ui})");
            let addon = format!("2.0.0(ui@{ui})");
            importers[i] = json!({"dependencies":{"host":{"specifier":"1.0.0","version":host}}});
            let mut deps = json!({"ui":ui});
            if installed && i == "a" {
                deps["addon"] = "1.0.0".into();
                snapshots["addon@1.0.0"] = json!({});
            }
            snapshots[format!("host@{host}")] = json!({"dependencies":deps});
            snapshots[format!("ui@{ui}")] = json!({});
            if i == "a" || second_witness {
                importers[i]["dependencies"]["addon"] =
                    json!({"specifier":"2.0.0","version":addon});
                snapshots[format!("addon@{addon}")] = json!({"dependencies":{"ui":ui}});
            }
        }
        let mut doc = lock(importers, snapshots).project().unwrap().clone();
        doc["overrides"] = overrides.clone();
        for pkg in ["addon@2.0.0", "host@1.0.0"] {
            doc["packages"][pkg]["peerDependencies"] = json!({"ui":"*"});
        }
        if installed {
            doc["packageExtensionsChecksum"] =
                format!("{:x}", Md5::digest(extension.to_string())).into();
        }
        parse(doc)
    };
    let base = make(false, true);
    let installed = make(true, true);
    let result = converge(&base, &installed, &[], options).unwrap();
    result.validate().unwrap();
    for ui in ["1.0.0", "2.0.0"] {
        assert_eq!(
            result.project().unwrap()["snapshots"][format!("host@1.0.0(ui@{ui})")]["dependencies"],
            json!({"ui":ui,"addon":format!("2.0.0(ui@{ui})")})
        );
    }
    assert_eq!(
        converge(&base, &result, &[], options)
            .unwrap()
            .to_yaml()
            .unwrap(),
        result.to_yaml().unwrap()
    );

    let base = make(false, false);
    let installed = make(true, false);
    installed.validate().unwrap();
    assert!(
        matches!(converge(&base,&installed,&[],options), Err(LockfileError::ResolutionRequired { path, .. }) if path.last().is_some_and(|n| n=="addon"))
    );
}

#[test]
fn an_unchanged_override_keeps_the_installed_correction_and_its_required_subtree() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    use md5::{Digest, Md5};

    let extension = json!({"host@1.0.0":{"dependencies":{"addon":"1.0.0"}}});
    let overrides = json!({"host>addon":"2.0.0"});
    let config =
        WorkspaceConfig::from_pnpm(&json!({"overrides":overrides,"packageExtensions":extension}))
            .unwrap();
    let options = ConvergeOptions {
        baseline_configuration: Some(&config),
        configuration: Some(&config),
        ..Default::default()
    };
    let make = |installed: bool| {
        let drift = if installed { "1.1.0" } else { "1.0.0" };
        let (addon, body) = if installed {
            ("2.0.0", json!({"dependencies":{"leaf":"3.0.0"}}))
        } else {
            ("1.0.0", json!({}))
        };
        let mut snaps = json!({"host@1.0.0":{"dependencies":{"addon":addon,"drift":drift}},
            format!("addon@{addon}"):body,format!("drift@{drift}"):{}});
        if installed {
            snaps["leaf@3.0.0"] = json!({});
        }
        let source = lock(
            json!({".":{"dependencies":{"host":{"specifier":"1.0.0","version":"1.0.0"}}}}),
            snaps,
        );
        let mut doc = source.project().unwrap().clone();
        doc["overrides"] = overrides.clone();
        doc["packageExtensionsChecksum"] =
            format!("{:x}", Md5::digest(extension.to_string())).into();
        parse(doc)
    };
    let base = make(false);
    let installed = make(true);
    let result = converge(&base, &installed, &[], options).unwrap();
    assert_eq!(
        result.project().unwrap()["snapshots"],
        json!({
            "host@1.0.0":{"dependencies":{"addon":"2.0.0","drift":"1.0.0"}},
            "addon@2.0.0":{"dependencies":{"leaf":"3.0.0"}},
            "drift@1.0.0":{}, "leaf@3.0.0":{}
        })
    );
    result.validate().unwrap();
    assert_eq!(
        converge(&base, &result, &[], options)
            .unwrap()
            .to_yaml()
            .unwrap(),
        result.to_yaml().unwrap()
    );
}

#[test]
fn converge_adds_and_removes_catalog_entries_when_workspace_config_is_unchanged() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};

    let config =
        WorkspaceConfig::from_pnpm(&json!({"catalogs":{"tools":{"pkg":"^1.0.0"}}})).unwrap();

    let without_consumer = parse(json!({
        "lockfileVersion": "9.0",
        "importers": {".": {}, "app": {}},
        "packages": {},
        "snapshots": {}
    }));
    let with_consumer = parse(json!({
        "lockfileVersion": "9.0",
        "catalogs": {"tools": {"pkg": {"specifier": "^1.0.0", "version": "1.0.1"}}},
        "importers": {".": {}, "app": {"dependencies": {"pkg": {"specifier": "catalog:tools", "version": "1.0.1"}}}},
        "packages": {"pkg@1.0.1": {"resolution": {"integrity": "sha512-pkg"}}},
        "snapshots": {"pkg@1.0.1": {}}
    }));

    let unused_manifest = json!({});
    let consumer_manifest = json!({"dependencies": {"pkg": "catalog:tools"}});

    for baseline_configuration in [Some(&config), None] {
        let options = ConvergeOptions {
            baseline_configuration,
            configuration: Some(&config),
            ..Default::default()
        };
        let added = converge(
            &without_consumer,
            &with_consumer,
            &[change("app", &unused_manifest, &consumer_manifest)],
            options,
        )
        .unwrap();
        assert_eq!(
            added.project().unwrap()["catalogs"],
            json!({"tools": {"pkg": {"specifier": "^1.0.0", "version": "1.0.1"}}})
        );

        let removed = converge(
            &with_consumer,
            &without_consumer,
            &[change("app", &consumer_manifest, &unused_manifest)],
            options,
        )
        .unwrap();
        assert!(removed.project().unwrap().get("catalogs").is_none());
    }
}

#[test]
fn converge_adds_a_catalog_consumer_linked_to_another_importer() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};

    let config = WorkspaceConfig::from_pnpm(
        &json!({"catalogs":{"tools":{"pkg":"^1.0.0"}}, "linkWorkspacePackages":true}),
    )
    .unwrap();
    let options = ConvergeOptions {
        configuration: Some(&config),
        ..Default::default()
    };
    let baseline = parse(json!({
        "lockfileVersion": "9.0", "importers": {".": {}, "app": {}, "packages/pkg": {}}
    }));
    let mut installed = json!({
        "lockfileVersion": "9.0",
        "catalogs": {"tools": {"pkg": {"specifier": "^1.0.0", "version": "1.0.1"}}},
        "importers": {".": {}, "packages/pkg": {}, "app": {"dependencies": {
            "pkg": {"specifier": "catalog:tools", "version": "link:../packages/pkg"}
        }}}
    });
    let before = json!({});
    let after = json!({"dependencies":{"pkg":"catalog:tools"}});
    let changes = [change("app", &before, &after)];
    let linked = converge(&baseline, &parse(installed.clone()), &changes, options).unwrap();
    assert_eq!(
        linked.project().unwrap()["importers"]["app"]["dependencies"]["pkg"]["version"],
        "link:../packages/pkg"
    );
    assert_eq!(linked.project().unwrap()["catalogs"], installed["catalogs"]);
    linked.validate().unwrap();

    installed["importers"]["app"]["dependencies"]["pkg"]["version"] = "link:../missing".into();
    assert!(converge(&baseline, &parse(installed), &changes, options).is_err());
}

#[test]
fn inferred_catalog_refresh_keeps_the_baseline_pin_with_equivalent_settings() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    let config = WorkspaceConfig::from_pnpm(&json!({"catalog":{"dep":"^1"}})).unwrap();
    let options = ConvergeOptions {
        configuration: Some(&config),
        ..Default::default()
    };
    let project = |version: &str| {
        json!({
            "lockfileVersion":"9.0",
            "catalogs":{"default":{"dep":{"specifier":"^1","version":version}}},
            "importers":{".":{"dependencies":{"dep":{"specifier":"catalog:","version":version}}}},
            "packages":{format!("dep@{version}"):{"resolution":{}}},
            "snapshots":{format!("dep@{version}"):{}}
        })
    };
    let baseline = parse(project("1.0.1"));
    let mut donor = project("1.0.2");
    donor["settings"] = json!({"autoInstallPeers":true,"excludeLinksFromLockfile":false});
    let converged = converge(&baseline, &parse(donor.clone()), &[], options).unwrap();
    assert_eq!(converged.project(), baseline.project());
    donor["settings"]["autoInstallPeers"] = false.into();
    assert!(matches!(converge(&baseline, &parse(donor), &[], options),
        Err(LockfileError::ResolutionRequired { path, .. }) if path == ["configuration"]
    ));
}

#[test]
fn inferred_catalog_projection_checks_the_retained_bootstrap_graph() {
    use locksmith::{ConvergeOptions, DocumentKind, WorkspaceConfig};
    let config = WorkspaceConfig::from_pnpm(&json!({"catalog":{"dep":"^1"}})).unwrap();
    let options = ConvergeOptions {
        configuration: Some(&config),
        ..Default::default()
    };
    let lock = |version: &str, broken: bool| {
        let environment = json!({
            "lockfileVersion":"9.0",
            "importers":{".":{"configDependencies":{}}},
            "packages":{"env@1.0.0":{"resolution":{}}},
            "snapshots":{"env@1.0.0": if broken { json!({"dependencies":{"missing":"1.0.0"}}) } else { json!({}) }}
        });
        let project = json!({
            "lockfileVersion":"9.0",
            "catalogs":{"default":{"dep":{"specifier":"^1","version":version}}},
            "importers":{".":{"dependencies":{"dep":{"specifier":"catalog:","version":version}}}},
            "packages":{format!("dep@{version}"):{"resolution":{}}},
            "snapshots":{format!("dep@{version}"):{}}
        });
        Lockfile::parse(&format!("{environment}\n---\n{project}\n")).unwrap()
    };
    let baseline = lock("1.0.1", false);
    let donor = lock("1.0.2", false);
    let result = converge(&baseline, &donor, &[], options).unwrap();
    assert_eq!(result.environment(), baseline.environment());
    assert!(matches!(
        converge(&lock("1.0.1", true), &donor, &[], options),
        Err(LockfileError::InvalidGraph(issues)) if issues.iter().any(|issue| issue.document == DocumentKind::Environment && issue.path == ["snapshots", "env@1.0.0", "missing"])
    ));
}

#[test]
fn catalog_projection_rejects_links_to_the_wrong_existing_destination() {
    use locksmith::{ConvergeOptions, WorkspaceConfig};
    for (runner, root_relative) in [((11, 28, 12), false), ((12, 21, 0), true)] {
        let config = |destination| {
            WorkspaceConfig::from_pnpm_version(
                &json!({"catalog": {"dep":format!("workspace:./packages/{destination}")}}),
                &runner.into(),
            )
            .unwrap()
        };
        let [before, after] = [config("old"), config("new")];
        let options = ConvergeOptions {
            baseline_configuration: Some(&before),
            configuration: Some(&after),
            ..Default::default()
        };
        let reference = |destination| {
            let prefix = if root_relative { "../" } else { "" };
            format!("link:{prefix}packages/{destination}")
        };
        let lock = |destination, pin| {
            let spec = format!("workspace:./packages/{destination}");
            parse(json!({
                "lockfileVersion":"9.0",
                "catalogs":{"default":{"dep":{"specifier":spec,"version":spec}}},
                "importers":{
                    ".":{},"packages/old":{},"packages/new":{},"app/packages/old":{},"app/packages/new":{},
                    "app":{"dependencies":{"dep":{"specifier":"catalog:","version":pin}}}
                }
            }))
        };
        let base = lock("old", reference("old"));
        let result = converge(&base, &lock("new", reference("new")), &[], options).unwrap();
        assert_eq!(
            result.project().unwrap()["importers"]["app"]["dependencies"]["dep"]["version"],
            reference("new")
        );
        assert!(matches!(
            converge(&base, &lock("new", reference("old")), &[], options),
            Err(LockfileError::ResolutionRequired { path, .. }) if path == ["importers", "app", "dependencies", "dep"]
        ));
    }
}
