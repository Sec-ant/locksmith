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
        registry: None,
        target: MergeTarget::Ours,
        manifests: &manifests,
        configuration: &cfg,
        configurations: [&cfg; 3],
        patches: &BTreeMap::new(),
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
    let manifests = BTreeMap::from([
        (".".to_owned(), json!({})),
        (
            "a".to_owned(),
            json!({"dependencies":{"widget":"1","plugin":"1.0.0"}}),
        ),
        ("b".to_owned(), json!({"dependencies":{"widget":"1"}})),
    ]);
    let lock = Lockfile::parse(
        &json!({"lockfileVersion":"9.0","importers":{".":{},"a":{},"b":{}}}).to_string(),
    )
    .unwrap();
    let cfg = WorkspaceConfig::default();
    let mut data = RegistryMetadata::default();
    for (name, selector) in [("widget", "1"), ("plugin", "1.0.0")] {
        let mut package = json!({"name":name,"version":"1.0.0","dist":{"integrity":"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=","tarball":format!("https://example.org/{name}/-/{name}-1.0.0.tgz")}});
        if name == "widget" {
            package["peerDependencies"] = json!({"plugin":"^1"});
            package["peerDependenciesMeta"] = json!({"plugin":{"optional":true}});
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
            registry: Some(&data),
            target: MergeTarget::Ours,
            configuration: &cfg,
            configurations: [&cfg; 3],
            manifests: &manifests,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    let doc = graph.project().unwrap();
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
            registry: None,
            target: MergeTarget::Ours,
            configuration: &cfg,
            configurations: [&cfg; 3],
            manifests: &manifests,
            patches: &BTreeMap::new(),
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
