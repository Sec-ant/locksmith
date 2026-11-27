use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeTarget, RegistryMetadata, RegistryRequest,
    WorkspaceConfig, merge,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn published_dependency_uses_local_children_and_separate_peer_instances() {
    let mut graph = json!({"lockfileVersion":"9.0","importers":{".":{}},"packages":{"helper@1.2.0":{"resolution":{"integrity":"sha512-helper"}}},"snapshots":{"helper@1.2.0":{}}});
    let mut manifests = BTreeMap::from([(".".into(), json!({}))]);
    for (id, v) in [("one", "1.0.0"), ("two", "2.0.0")] {
        graph["importers"][id] = json!({"dependencies":{"host":{"specifier":v,"version":v}}});
        graph["packages"][format!("host@{v}")] = json!({"resolution":{"integrity":"sha512-host"}});
        graph["snapshots"][format!("host@{v}")] = json!({});
        manifests.insert(
            id.into(),
            json!({"dependencies":{"host":v,"widget":"1.0.0"}}),
        );
    }
    let lock = Lockfile::parse(&graph.to_string()).unwrap();
    let cfg = WorkspaceConfig::default();
    let ctx = MergeContext {
        target: MergeTarget::Ours,
        manifests: &manifests,
        configurations: [&cfg; 3],
        configuration: &cfg,
        patches: &BTreeMap::new(),
        registry: None,
    };
    let err = merge(&lock, &lock, &lock, Some(&ctx)).unwrap_err();
    let LockfileError::ResolutionRequired {
        request: Some(request),
        ..
    } = err
    else {
        panic!("{err}")
    };
    assert!(
        matches!(request.as_ref(), RegistryRequest::Version{name,selector} if name=="widget" && selector=="1.0.0")
    );
    let mut registry = RegistryMetadata::default();
    registry.insert(&request,json!({"name":"widget","version":"1.0.0","dependencies":{"h":"npm:helper@^1"},"peerDependencies":{"host":"*","optional-host":"*"},"peerDependenciesMeta":{"optional-host":{"optional":true}},"dist":{"integrity":"sha512-T06YHNwS+4WJyrYrEWmZa+Iy87bJlWYDYrMBD48tiSqTPPrkMmfPy9zmtJ8bPYCpkFWc4CiQXk79Aml3c+rwvA==","tarball":"https://registry.example/widget/-/widget-1.0.0.tgz"}}),"https://registry.example/",false).unwrap();
    let ctx = MergeContext {
        registry: Some(&registry),
        ..ctx
    };
    let out = merge(&lock, &lock, &lock, Some(&ctx)).unwrap();
    out.validate().unwrap();
    let doc = out.project().unwrap();
    for (id, v) in [("one", "1.0.0"), ("two", "2.0.0")] {
        let instance = format!("1.0.0(host@{v})");
        assert_eq!(
            doc["importers"][id]["dependencies"]["widget"]["version"],
            instance
        );
        assert_eq!(
            doc["snapshots"][format!("widget@{instance}")]["dependencies"],
            json!({"h":"helper@1.2.0","host":v})
        );
    }
}

#[test]
fn newly_published_optional_peer_uses_the_workspace_provider_and_optional_edge() {
    for (explicit, ignored) in [(Some("^1"), false), (None, false), (None, true)] {
        let manifests = BTreeMap::from([
            (".".to_owned(), json!({})),
            (
                "a".to_owned(),
                json!({"dependencies":{"widget":"1","plugin":"1.0.0"}}),
            ),
            ("b".to_owned(), json!({"dependencies":{"widget":"1"}})),
        ]);
        let mut doc = json!({"lockfileVersion":"9.0","importers":{".":{},"a":{},"b":{}}});
        let cfg = if ignored {
            let settings = json!({"ignoredOptionalDependencies":["plug*","!plugin-tools"]});
            doc["ignoredOptionalDependencies"] = settings["ignoredOptionalDependencies"].clone();
            WorkspaceConfig::from_pnpm(&settings).unwrap()
        } else {
            WorkspaceConfig::default()
        };
        let lock = Lockfile::parse(&doc.to_string()).unwrap();
        let mut data = RegistryMetadata::default();
        for (name, selector) in [("widget", "1"), ("plugin", "1.0.0")] {
            let mut package = json!({"name":name,"version":"1.0.0","dist":{"integrity":"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=","tarball":format!("https://example.org/{name}/-/{name}-1.0.0.tgz")}});
            if name == "widget" {
                package["peerDependencies"] = json!({"widget":"^1","undeclared":"not a range"});
                if let Some(range) = explicit {
                    package["peerDependencies"]["plugin"] = range.into();
                }
                if ignored {
                    package["optionalDependencies"] = json!({"plugin":"^2"});
                }
                package["peerDependenciesMeta"] =
                    json!({"plugin":{"optional":true},"undeclared":{"optional":true}});
            }
            data.insert(
                &RegistryRequest::Version {
                    name: name.into(),
                    selector: selector.into(),
                },
                package,
                "https://example.org/",
                false,
            )
            .unwrap();
        }
        let graph = merge(
            &lock,
            &lock,
            &lock,
            Some(&MergeContext {
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&cfg; 3],
                configuration: &cfg,
                patches: &BTreeMap::new(),
                registry: Some(&data),
            }),
        )
        .unwrap();
        let doc = graph.project().unwrap();
        assert_eq!(
            doc["packages"]["widget@1.0.0"]["peerDependencies"],
            json!({"plugin":explicit.unwrap_or("*"),"undeclared":"not a range"})
        );
        for id in ["a", "b"] {
            assert_eq!(
                doc["importers"][id]["dependencies"]["widget"]["version"],
                "1.0.0(plugin@1.0.0)"
            );
        }
        assert_eq!(
            doc["snapshots"]["widget@1.0.0(plugin@1.0.0)"],
            json!({"optionalDependencies":{"plugin":"1.0.0"}})
        );
    }
}

#[test]
fn published_optional_peer_is_restored_after_its_dependency_is_removed() {
    let required = json!({"dependencies":{"plugin":"1.0.0"}});
    let bundled = json!({"dependencies":{"plugin":"1.0.0"},"bundledDependencies":["plugin"]});
    let bundled_optional =
        json!({"optionalDependencies":{"plugin":"1.0.0"},"bundledDependencies":["plugin"]});
    let duplicate =
        json!({"dependencies":{"plugin":"1.0.0"},"optionalDependencies":{"plugin":"1.0.0"}});
    for (case, fields, remove, ignore, expect_peer) in [
        ("removed", &required, true, false, true),
        ("kept", &required, false, false, false),
        ("bundled", &bundled, false, false, false),
        ("bundled and removed", &bundled, true, false, true),
        ("bundled optional", &bundled_optional, false, false, false),
        (
            "bundled optional and removed",
            &bundled_optional,
            true,
            false,
            true,
        ),
        (
            "bundled optional and ignored",
            &bundled_optional,
            false,
            true,
            true,
        ),
        ("duplicate and ignored", &duplicate, false, true, true),
    ] {
        let mut settings = json!({});
        if remove {
            settings["overrides"] = json!({"widget>plugin":"-"});
        }
        if ignore {
            settings["ignoredOptionalDependencies"] = json!(["plugin"]);
        }
        let cfg =
            WorkspaceConfig::from_pnpm_version(&settings, &"10.34.9".parse().unwrap()).unwrap();
        let mut doc = settings;
        doc["lockfileVersion"] = "9.0".into();
        doc["importers"] = json!({".":{}});
        let lock = Lockfile::parse(&doc.to_string()).unwrap();
        let mut data = RegistryMetadata::default();
        for (name, version) in [
            ("widget", "1.0.0"),
            ("plugin", "1.0.0"),
            ("plugin", "2.0.0"),
        ] {
            let mut package = json!({"name":name,"version":version,"dist":{
                "integrity":"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
                "tarball":format!("https://example.org/{name}/-/{name}-{version}.tgz")
            }});
            if name == "widget" {
                package
                    .as_object_mut()
                    .unwrap()
                    .extend(fields.as_object().unwrap().clone());
                package["peerDependenciesMeta"] = json!({"plugin":{"optional":true}});
            }
            data.insert(
                &RegistryRequest::Version {
                    name: name.into(),
                    selector: version.into(),
                },
                package,
                "https://example.org/",
                false,
            )
            .unwrap();
        }
        let manifests = BTreeMap::from([(
            ".".into(),
            json!({"dependencies":{"widget":"1.0.0","plugin":"2.0.0"}}),
        )]);
        let result = merge(
            &lock,
            &lock,
            &lock,
            Some(&MergeContext {
                target: MergeTarget::Ours,
                manifests: &manifests,
                configurations: [&cfg; 3],
                configuration: &cfg,
                patches: &BTreeMap::new(),
                registry: Some(&data),
            }),
        )
        .unwrap();
        let doc = result.project().unwrap();
        let package = &doc["packages"]["widget@1.0.0"];
        if expect_peer {
            assert_eq!(package["peerDependencies"], json!({"plugin":"*"}), "{case}");
            assert_eq!(
                package["peerDependenciesMeta"],
                json!({"plugin":{"optional":true}}),
                "{case}"
            );
            assert_eq!(
                doc["snapshots"]["widget@1.0.0(plugin@2.0.0)"],
                json!({"optionalDependencies":{"plugin":"2.0.0"}}),
                "{case}"
            );
        } else {
            assert!(package.get("peerDependencies").is_none(), "{case}");
            assert!(package.get("peerDependenciesMeta").is_none(), "{case}");
        }
    }
}

#[test]
fn published_optional_dependency_takes_precedence_over_an_overridden_peer() {
    let mut registry = RegistryMetadata::default();
    for (name, version) in [
        ("widget", "1.0.0"),
        ("plugin", "1.0.0"),
        ("plugin", "2.0.0"),
        ("replacement", "1.0.0"),
    ] {
        let mut package = json!({"name":name,"version":version,"dist":{
            "integrity":"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "tarball":format!("https://example.org/{name}/-/{name}-{version}.tgz")
        }});
        if name == "widget" {
            package["optionalDependencies"] = json!({"plugin":"2.0.0"});
            package["peerDependencies"] = json!({"plugin":"1.0.0"});
        }
        registry
            .insert(
                &RegistryRequest::Version {
                    name: name.into(),
                    selector: version.into(),
                },
                package,
                "https://example.org/",
                false,
            )
            .unwrap();
    }

    for (version, optional_pin) in [
        ("10.34.9", Some("2.0.0")),
        ("12.15.0", Some("replacement@1.0.0")),
        ("12.21.0", Some("2.0.0")),
        ("12.15.0", None),
        ("12.21.0", None),
    ] {
        let overrides = json!({"widget>plugin@1.0.0":"npm:replacement@1.0.0"});
        let mut lock = json!({"lockfileVersion":"9.0","importers":{".":{}},"overrides":overrides});
        let mut settings = json!({"overrides":overrides});
        if optional_pin.is_none() {
            settings["ignoredOptionalDependencies"] = json!(["plugin"]);
            lock["ignoredOptionalDependencies"] = json!(["plugin"]);
        }
        let lock = Lockfile::parse(&lock.to_string()).unwrap();
        let cfg = WorkspaceConfig::from_pnpm_version(&settings, &version.parse().unwrap()).unwrap();
        let out = merge(
            &lock,
            &lock,
            &lock,
            Some(&MergeContext {
                target: MergeTarget::Ours,
                manifests: &BTreeMap::from([(
                    ".".into(),
                    json!({"dependencies":{"widget":"1.0.0"}}),
                )]),
                configurations: [&cfg; 3],
                configuration: &cfg,
                patches: &BTreeMap::new(),
                registry: Some(&registry),
            }),
        )
        .unwrap();
        let doc = out.project().unwrap();
        let instance = doc["importers"]["."]["dependencies"]["widget"]["version"]
            .as_str()
            .unwrap();
        let body = &doc["snapshots"][format!("widget@{instance}")];
        let peers = &doc["packages"]["widget@1.0.0"]["peerDependencies"];
        if let Some(pin) = optional_pin {
            assert_eq!(instance, "1.0.0");
            assert_eq!(
                body,
                &json!({"optionalDependencies":{"plugin":pin}}),
                "{version}"
            );
            assert!(peers.is_null());
        } else {
            assert_eq!(instance, "1.0.0(plugin@1.0.0)");
            assert_eq!(body, &json!({"dependencies":{"plugin":"1.0.0"}}));
            assert_eq!(peers, &json!({"plugin":"1.0.0"}));
        }
    }
}

#[test]
fn auto_dedupe_combines_new_transitive_versions_only_when_the_parent_accepts_them() {
    const SRI: &str = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
    let base = json!({"lockfileVersion":"9.0","importers":{".":{}}});
    let side = |name: &str, child: &str| {
        let mut lock = base.clone();
        lock["importers"]["."]["dependencies"][name] =
            json!({"specifier":"1.0.0","version":"1.0.0"});
        for key in [format!("{name}@1.0.0"), format!("child@{child}")] {
            lock["packages"][&key] = json!({"resolution":{"integrity":SRI}});
            lock["snapshots"][&key] = json!({});
        }
        lock["snapshots"][format!("{name}@1.0.0")]["dependencies"] = json!({"child":child});
        Lockfile::parse(&lock.to_string()).unwrap()
    };
    let ours = side("parent", "1.0.0");
    let theirs = side("extra", "1.1.0");
    let base = Lockfile::parse(&base.to_string()).unwrap();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"parent":"1.0.0","extra":"1.0.0"}}),
    )]);
    for (enabled, range, expected) in [
        (false, "^1", "1.0.0"),
        (true, "^1", "1.1.0"),
        (true, "1.0.0", "1.0.0"),
    ] {
        let cfg = WorkspaceConfig::from_pnpm(&json!({"autoDedupe":enabled})).unwrap();
        let ctx = MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&cfg; 3],
            configuration: &cfg,
            patches: &BTreeMap::new(),
            registry: None,
        };
        let mut data = RegistryMetadata::default();
        if enabled {
            let err = merge(&base, &ours, &theirs, Some(&ctx)).unwrap_err();
            let LockfileError::ResolutionRequired {
                request: Some(req), ..
            } = err
            else {
                panic!("{err}");
            };
            assert!(
                matches!(&*req, RegistryRequest::Manifest {name,version} if name=="parent" && version=="1.0.0")
            );
            data.insert(&req, json!({
                "name":"parent","version":"1.0.0","dependencies":{"child":range},
                "dist":{"integrity":SRI,"tarball":"https://example.org/parent/-/parent-1.0.0.tgz"}
            }), "https://example.org/", false).unwrap();
        }
        let result = merge(
            &base,
            &ours,
            &theirs,
            Some(&MergeContext {
                registry: Some(&data),
                ..ctx
            }),
        )
        .unwrap();
        result.validate().unwrap();
        let doc = result.project().unwrap();
        assert_eq!(
            doc["snapshots"]["parent@1.0.0"]["dependencies"]["child"],
            expected
        );
        assert_eq!(
            doc["packages"].get("child@1.0.0").is_some(),
            expected == "1.0.0"
        );
    }
}

#[test]
fn registry_requests_cover_independent_missing_pins_across_workspace_members() {
    let lock = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}}\n").unwrap();
    let cfg = WorkspaceConfig::default();
    let manifests = BTreeMap::from([
        (
            ".".into(),
            json!({"dependencies":{"first":"1.0.0","local":"link:./vendor"}}),
        ),
        ("app".into(), json!({"dependencies":{"second":"2.0.0"}})),
    ]);
    let ctx = MergeContext {
        target: MergeTarget::Ours,
        manifests: &manifests,
        configurations: [&cfg; 3],
        configuration: &cfg,
        patches: &BTreeMap::new(),
        registry: None,
    };
    let errors = ctx.merge_with_requests([&lock; 3]).unwrap_err();
    let requests: BTreeMap<_, _> = errors
        .into_iter()
        .map(|error| {
            let LockfileError::ResolutionRequired {
                request: Some(req), ..
            } = error
            else {
                panic!("expected a registry lookup, got {error}")
            };
            (req.name().to_owned(), req.selector().to_owned())
        })
        .collect();
    assert_eq!(
        requests,
        BTreeMap::from([
            ("first".into(), "1.0.0".into()),
            ("second".into(), "2.0.0".into())
        ])
    );
}

#[test]
fn published_sibling_dependencies_are_requested_in_one_batch() {
    for group in ["dependencies", "peerDependencies"] {
        let mut registry = RegistryMetadata::default();
        let request = RegistryRequest::Version {
            name: "parent".into(),
            selector: "1.0.0".into(),
        };
        registry.insert(&request, json!({"name":"parent","version":"1.0.0",group:{"a":"1","b":"2"},"dist":{"shasum":"0123456789abcdef0123456789abcdef01234567","tarball":"https://example.org/parent/-/parent-1.0.0.tgz"}}),"https://example.org",false).unwrap();
        let lock = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}}\n").unwrap();
        let cfg = WorkspaceConfig::default();
        let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"parent":"1.0.0"}}))]);
        let ctx = MergeContext {
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&cfg; 3],
            configuration: &cfg,
            patches: &BTreeMap::new(),
            registry: Some(&registry),
        };
        let mut names = std::collections::BTreeSet::new();
        for error in ctx.merge_with_requests([&lock; 3]).unwrap_err() {
            let LockfileError::ResolutionRequired {
                path,
                request: Some(request),
                ..
            } = error
            else {
                panic!("{error}")
            };
            assert_eq!(path, ["snapshots", "parent@1.0.0", request.name()]);
            names.insert(request.name().to_owned());
        }
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["a".into(), "b".into()])
        );
    }
}

#[test]
fn terminal_peer_errors_take_precedence_over_pending_registry_requests() {
    let mut registry = RegistryMetadata::default();
    for (name, group, dependency, spec) in [
        ("a", "peerDependencies", "p", "catalog:missing"),
        ("b", "dependencies", "q", "1.0.0"),
    ] {
        let request = RegistryRequest::Version {
            name: name.into(),
            selector: "1.0.0".into(),
        };
        registry.insert(&request, json!({"name":name,"version":"1.0.0",
            group:{dependency:spec},"dist":{"shasum":"0123456789abcdef0123456789abcdef01234567",
                "tarball":format!("https://example.org/{name}/-/{name}-1.0.0.tgz")}
        }), "https://example.org", false).unwrap();
    }
    let lock = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}}\n").unwrap();
    let config = WorkspaceConfig::default();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"1.0.0","b":"1.0.0"}}),
    )]);
    let context = MergeContext {
        target: MergeTarget::Ours,
        manifests: &manifests,
        configurations: [&config; 3],
        configuration: &config,
        patches: &BTreeMap::new(),
        registry: Some(&registry),
    };
    let errors = context.merge_with_requests([&lock; 3]).unwrap_err();
    assert!(
        matches!(
            errors.as_slice(),
            [LockfileError::ResolutionRequired { path, request: None, .. }]
                if path == &["snapshots", "a@1.0.0", "p"]
        ),
        "{errors:?}"
    );
}

#[test]
fn published_optional_peer_rebinds_and_alias_full_keys_survive_expansion() {
    let mut registry = RegistryMetadata::default();
    let request = RegistryRequest::Version {
        name: "widget".into(),
        selector: "1.0.0".into(),
    };
    registry
        .insert(
            &request,
            json!({
                "name": "widget",
                "version": "1.0.0",
                "peerDependencies": {"plugin": "^1"},
                "peerDependenciesMeta": {"plugin": {"optional": true}},
                "dist": {
                    "shasum": "0123456789abcdef0123456789abcdef01234567",
                    "tarball": "https://example.org/widget/-/widget-1.0.0.tgz"
                }
            }),
            "https://example.org",
            false,
        )
        .unwrap();
    let make = |helper_version: &str| {
        let plugin_key = format!("plugin@1.0.0(helper@{helper_version})");
        Lockfile::parse(
            &json!({
                "lockfileVersion": "9.0",
                "importers": {
                    ".": {
                        "dependencies": {
                            "helper": {"specifier": helper_version, "version": helper_version},
                            "plugin": {"specifier": "1.0.0", "version": format!("1.0.0(helper@{helper_version})")}
                        }
                    }
                },
                "packages": {
                    format!("helper@{helper_version}"): {"resolution": {"integrity": "sha512-helper"}},
                    "plugin@1.0.0": {"resolution": {"integrity": "sha512-plugin"}, "peerDependencies": {"helper": "^1 || ^2"}}
                },
                "snapshots": {
                    format!("helper@{helper_version}"): {},
                    plugin_key: {"dependencies": {"helper": helper_version}}
                }
            })
            .to_string(),
        )
        .unwrap()
    };
    let base = make("1.0.0");
    let ours = make("2.0.0");
    let config = WorkspaceConfig::default();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies": {"helper": "2.0.0", "plugin": "1.0.0", "widget": "1.0.0"}}),
    )]);
    let result = merge(
        &base,
        &ours,
        &base,
        Some(&MergeContext {
            target: MergeTarget::Theirs,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &BTreeMap::new(),
            registry: Some(&registry),
        }),
    )
    .unwrap();
    result.validate().unwrap();
    let doc = result.project().unwrap();
    assert_eq!(
        doc["snapshots"]["widget@1.0.0(plugin@1.0.0(helper@2.0.0))"]["optionalDependencies"]["plugin"],
        "1.0.0(helper@2.0.0)"
    );
}
