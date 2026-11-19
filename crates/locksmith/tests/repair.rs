use locksmith::{Lockfile, LockfileError, RepairContext, Value, WorkspaceConfig, repair};
use serde_json::json;
use std::collections::BTreeMap;

const LOCK: &str = r#"# application
lockfileVersion: '9.0'

catalogs:
  default:
    chosen:
      specifier: ^1.0.0
      version: 1.0.0

importers:

  .:
    dependencies:
      chosen:
        specifier: 'catalog:'
        version: 1.0.1
      stable:
        specifier: ^2.0.0
        version: 2.0.2

packages:

  chosen@1.0.0:
    resolution: {integrity: sha512-a}

  chosen@1.0.1:
    resolution: {integrity: sha512-b}

  stable@2.0.2:
    resolution: {integrity: sha512-c}

snapshots:

  chosen@1.0.0: {}

  chosen@1.0.1: {}

  stable@2.0.2: {}
"#;

fn run(text: &str, range: &str, parent: Option<&Lockfile>) -> Result<String, LockfileError> {
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"chosen":"catalog:","stable":"^2.0.0"}}),
    )]);
    let config = WorkspaceConfig::from_pnpm(&json!({"catalog":{"chosen":range}})).unwrap();
    let parents = parent.into_iter().map(|p| (p, &config)).collect::<Vec<_>>();
    repair(
        text,
        &RepairContext {
            manifests: &manifests,
            configuration: &config,
            patches: &BTreeMap::new(),
            parents: &parents,
            registry: None,
        },
    )?
    .to_yaml()
}

#[test]
fn a_valid_catalog_range_can_have_two_different_locked_versions() {
    let source = LOCK
        .replace(
            "stable@2.0.2: {}",
            "\"stable@2.0.2\": {}  # leave as written",
        )
        .replace('\n', "\r\n");
    assert_eq!(run(&source, "^1.0.0", None).unwrap(), source);
}

#[test]
fn changes_one_stale_catalog_pin_and_preserves_all_other_bytes() {
    let source = LOCK
        .replace("^1.0.0", "1.0.0")
        .replace("stable@2.0.2: {}", "\"stable@2.0.2\": {}  # unrelated")
        .replace('\n', "\r\n");
    let source = source.replace("        version: 1.0.1", "        version: 1.0.1  # pin");
    let expected = source.replace("        version: 1.0.1", "        version: 1.0.0");
    let output = run(&source, "1.0.0", None).unwrap();
    assert_eq!(output, expected);
    assert_eq!(run(&output, "1.0.0", None).unwrap(), output);
}

#[test]
fn removes_only_equal_package_and_snapshot_records() {
    let block = "  chosen@1.0.0:\n    resolution: {integrity: sha512-a}\n\n";
    let snapshot = "  chosen@1.0.0: {}\n\n";
    let source = LOCK
        .replace(block, &format!("{block}{block}"))
        .replace(snapshot, &format!("{snapshot}{snapshot}"));
    assert_eq!(run(&source, "^1.0.0", None).unwrap(), LOCK);
    let conflict = source.replacen(
        "resolution: {integrity: sha512-a}",
        "resolution: {integrity: sha512-conflicting}",
        1,
    );
    assert!(
        matches!(run(&conflict,"^1.0.0",None),Err(LockfileError::ResolutionRequired{path,..}) if path == ["packages","chosen@1.0.0"])
    );
    assert!(
        run(
            &source.replace(
                "resolution: {integrity: sha512-a}",
                "resolution: {integrity: x, integrity: y}"
            ),
            "^1.0.0",
            None
        )
        .is_err()
    );
}

#[test]
fn recovers_the_exact_missing_peer_snapshot_and_its_transitive_closure() {
    let before = LOCK.replace(
        "  chosen@1.0.1: {}",
        "  chosen@1.0.1:\n    dependencies:\n      child: 3.0.0(peer@1.0.0)",
    );
    let mut donor = Lockfile::parse(&before).unwrap().project().unwrap().clone();
    donor["snapshots"]["child@3.0.0(peer@1.0.0)"] = json!({"dependencies":{"peer":"1.0.0"}});
    donor["snapshots"]["peer@1.0.0"] = json!({});
    donor["packages"]["child@3.0.0"] =
        json!({"resolution": {"integrity":"sha512-child"},"peerDependencies":{"peer":"^1"}});
    donor["packages"]["peer@1.0.0"] = json!({"resolution": {"integrity":"sha512-peer"}});
    donor["importers"]["."]["dependencies"]["stable"]["version"] = json!("2.0.1");
    let parent = Lockfile::parse(&donor.to_string()).unwrap();
    let repaired = run(&before, "^1.0.0", Some(&parent)).unwrap();
    let graph: Value = Lockfile::parse(&repaired)
        .unwrap()
        .project()
        .unwrap()
        .clone();
    assert_eq!(graph["snapshots"], donor["snapshots"]);
    assert_eq!(graph["packages"], donor["packages"]);
    assert_eq!(
        graph["importers"],
        Lockfile::parse(&before).unwrap().project().unwrap()["importers"]
    );
    assert_eq!(run(&repaired, "^1.0.0", None).unwrap(), repaired);
}

#[test]
fn leaves_an_unreachable_old_snapshot_and_a_satisfied_uncached_catalog_alone() {
    let start = LOCK.find("catalogs:").unwrap();
    let end = LOCK.find("importers:").unwrap();
    let raw = format!(
        "{}{}\n  orphan@1.0.0: {{dependencies: {{lost: 2.0.0}}}}\n",
        &LOCK[..start],
        &LOCK[end..]
    );
    assert_eq!(run(&raw, "^1.0.0", None).unwrap(), raw);
}

#[test]
fn rewrites_only_the_invalid_scalar_in_a_flow_mapping() {
    let input = LOCK.replace("^1.0.0", "1.0.0").replace(
        "      chosen:\n        specifier: 'catalog:'\n        version: 1.0.1",
        "      chosen: {specifier: 'catalog:', version: '1.0.1'}",
    );
    let output = run(&input, "1.0.0", None).unwrap();
    assert_eq!(
        Lockfile::parse(&output).unwrap().project().unwrap()["importers"]["."]["dependencies"]["chosen"]
            ["version"],
        "1.0.0"
    );
    assert!(output.contains("chosen: {specifier: 'catalog:', version:"));
    assert!(output.ends_with(input.split_once("packages:").unwrap().1));
}

#[test]
fn changing_a_direct_provider_repairs_only_its_consumers_peer_identity() {
    let current = json!({
        "lockfileVersion":"9.0",
        "importers":{".":{"dependencies":{"engine":{"specifier":"1.0.0","version":"1.0.0"},"plugin":{"specifier":"1.0.0","version":"1.0.0(engine@1.0.0)"}}}},
        "packages":{"engine@1.0.0":{"resolution":{}},"engine@2.0.0":{"resolution":{}},"plugin@1.0.0":{"resolution":{},"peerDependencies":{"engine":"*"}}},
        "snapshots":{"engine@1.0.0":{},"engine@2.0.0":{},"plugin@1.0.0(engine@1.0.0)":{"dependencies":{"engine":"1.0.0"}},"plugin@1.0.0(engine@2.0.0)":{"dependencies":{"engine":"2.0.0"}}}
    });
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"engine":"2.0.0","plugin":"1.0.0"}}),
    )]);
    let repaired = repair(
        &current.to_string(),
        &RepairContext {
            manifests: &manifests,
            configuration: &WorkspaceConfig::default(),
            patches: &BTreeMap::new(),
            parents: &[],
            registry: None,
        },
    )
    .unwrap();
    let doc = repaired.project().unwrap();
    assert_eq!(
        doc["importers"]["."]["dependencies"]["plugin"]["version"],
        "1.0.0(engine@2.0.0)"
    );
    assert_eq!(doc["packages"], current["packages"]);
    assert_eq!(doc["snapshots"], current["snapshots"]);
}

#[test]
fn registry_evidence_restores_the_named_release_and_its_exact_optional_peer() {
    use locksmith::{RegistryMetadata, RegistryRequest};
    let input = LOCK.replace(
        "  chosen@1.0.1: {}",
        "  chosen@1.0.1:\n    dependencies:\n      transport: 3.0.0(stable@2.0.2)",
    );
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"chosen":"catalog:","stable":"^2.0.0"}}),
    )]);
    let cfg = WorkspaceConfig::from_pnpm(&json!({"catalog":{"chosen":"^1.0.0"}})).unwrap();
    let ctx = RepairContext {
        manifests: &manifests,
        configuration: &cfg,
        patches: &BTreeMap::new(),
        parents: &[],
        registry: None,
    };
    let LockfileError::ResolutionRequired {
        request: Some(request),
        ..
    } = repair(&input, &ctx).unwrap_err()
    else {
        panic!("expected an exact registry request")
    };
    assert_eq!(
        *request,
        RegistryRequest::Version {
            name: "transport".into(),
            selector: "3.0.0".into()
        }
    );
    let mut registry = RegistryMetadata::default();
    registry.insert(&request,json!({"name":"transport","version":"3.0.0","peerDependencies":{"stable":"^2"},"peerDependenciesMeta":{"stable":{"optional":true}},"dist":{"integrity":"sha512-T06YHNwS+4WJyrYrEWmZa+Iy87bJlWYDYrMBD48tiSqTPPrkMmfPy9zmtJ8bPYCpkFWc4CiQXk79Aml3c+rwvA==","tarball":"https://registry.example/transport/-/transport-3.0.0.tgz"}}),"https://registry.example/",false).unwrap();
    let output = repair(
        &input,
        &RepairContext {
            registry: Some(&registry),
            ..ctx
        },
    )
    .unwrap();
    let doc = output.project().unwrap();
    assert_eq!(
        doc["importers"],
        Lockfile::parse(&input).unwrap().project().unwrap()["importers"]
    );
    assert_eq!(
        doc["snapshots"]["transport@3.0.0(stable@2.0.2)"],
        json!({"optionalDependencies":{"stable":"2.0.2"}})
    );
}

#[test]
fn removes_an_obsolete_unused_catalog_entry_without_removing_a_valid_one() {
    let stale = LOCK.replace(
        "  default:\n",
        "  default:\n    old:\n      specifier: 0.5.0\n      version: 0.5.0\n",
    );
    assert_eq!(run(&stale, "^1.0.0", None).unwrap(), LOCK);
}

#[test]
fn repairs_a_catalog_snapshot_whose_own_pin_violates_its_recorded_range() {
    let expected = LOCK
        .replace("^1.0.0", "1.0.0")
        .replace("        version: 1.0.1", "        version: 1.0.0");
    let corrupt = expected
        .replace(
            "      version: 1.0.0\n\nimporters:",
            "      version: 1.0.1\n\nimporters:",
        )
        .replace("        version: 1.0.0", "        version: 1.0.1");
    assert_eq!(run(&corrupt, "1.0.0", None).unwrap(), expected);
}

#[test]
fn repairs_a_required_extension_in_each_consumers_peer_scope() {
    use md5::{Digest, Md5};

    let extension = json!({"host@1.0.0":{"dependencies":{"addon":"1.0.0"}}});
    let overrides = json!({"host@1>addon":"2.0.0"});
    let cfg =
        WorkspaceConfig::from_pnpm(&json!({"overrides":overrides,"packageExtensions":extension}))
            .unwrap();
    let manifests = BTreeMap::from([
        (".".into(), json!({})),
        ("a".into(), json!({"dependencies":{"host":"1.0.0"}})),
        ("b".into(), json!({"dependencies":{"host":"1.0.0"}})),
    ]);
    let ctx = RepairContext {
        manifests: &manifests,
        configuration: &cfg,
        patches: &BTreeMap::new(),
        parents: &[],
        registry: None,
    };
    let mut doc = json!({
        "lockfileVersion":"9.0", "overrides":overrides,
        "packageExtensionsChecksum":format!("{:x}",Md5::digest(extension.to_string())),
        "importers":{".":{}}, "packages":{}, "snapshots":{}
    });
    for name in [
        "addon@1.0.0",
        "addon@2.0.0",
        "host@1.0.0",
        "ui@1.0.0",
        "ui@2.0.0",
    ] {
        doc["packages"][name] = json!({"resolution":{"integrity":"sha512-example"}});
    }
    for name in ["addon@2.0.0", "host@1.0.0"] {
        doc["packages"][name]["peerDependencies"] = json!({"ui":"*"});
    }
    for (app, ui) in [("a", "1.0.0"), ("b", "2.0.0")] {
        let host = format!("1.0.0(ui@{ui})");
        doc["importers"][app] =
            json!({"dependencies":{"host":{"specifier":"1.0.0","version":host}}});
        doc["snapshots"][format!("host@{host}")] = json!({"dependencies":{"ui":ui}});
        doc["snapshots"][format!("addon@2.0.0(ui@{ui})")] = json!({"dependencies":{"ui":ui}});
        doc["snapshots"][format!("ui@{ui}")] = json!({});
    }
    doc["snapshots"]["addon@1.0.0"] = json!({});
    doc["snapshots"]["host@1.0.0(ui@1.0.0)"]["dependencies"]["addon"] = "1.0.0".into();
    let run = |d: &Value| repair(&serde_saphyr::to_string(d).unwrap(), &ctx);
    let result = run(&doc).unwrap();
    let mut expected = doc.clone();
    for ui in ["1.0.0", "2.0.0"] {
        expected["snapshots"][format!("host@1.0.0(ui@{ui})")]["dependencies"]["addon"] =
            format!("2.0.0(ui@{ui})").into();
    }
    assert_eq!(result.project(), Some(&expected));
    let output = result.to_yaml().unwrap();
    assert_eq!(repair(&output, &ctx).unwrap().to_yaml().unwrap(), output);

    let mut missing = doc.clone();
    missing["snapshots"]
        .as_object_mut()
        .unwrap()
        .remove("addon@2.0.0(ui@2.0.0)");
    assert!(
        matches!(run(&missing),Err(LockfileError::ResolutionRequired { path, .. }) if path==["snapshots","host@1.0.0(ui@2.0.0)","addon"])
    );

    doc["packages"]["plugin@1.0.0"] = json!({"resolution":{},"peerDependencies":{"addon":"*"}});
    doc["snapshots"]["plugin@1.0.0(addon@1.0.0)"] = json!({"dependencies":{"addon":"1.0.0"}});
    doc["snapshots"]["host@1.0.0(ui@1.0.0)"]["dependencies"]["plugin"] =
        "1.0.0(addon@1.0.0)".into();
    assert!(
        matches!(run(&doc),Err(LockfileError::ResolutionRequired { path, .. }) if path==["snapshots","host@1.0.0(ui@1.0.0)","plugin"])
    );
}
