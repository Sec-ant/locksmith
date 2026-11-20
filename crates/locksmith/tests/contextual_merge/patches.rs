use super::{cfg, lock};
use locksmith::{Lockfile, MergeContext, MergeTarget, WorkspaceConfig, merge};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn final_patch_contents_select_the_matching_instance_for_an_unversioned_selector() {
    let make = |hash: &str| {
        let mut data = lock(1, 1, false).project().unwrap().clone();
        let old = "lib@1.0.0(a@1.0.0)(b@1.0.0)";
        let body = data["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove(old)
            .unwrap();
        data["snapshots"][format!("lib@1.0.0(patch_hash={hash})(a@1.0.0)(b@1.0.0)")] = body;
        data["importers"]["."]["dependencies"]["lib"]["version"] =
            json!(format!("1.0.0(patch_hash={hash})(a@1.0.0)(b@1.0.0)"));
        data["patchedDependencies"] = json!({"lib":{"path":"patches/lib.patch","hash":hash}});
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let old = make("01d09d19c2139a46aebfb577780d123d7396e97201bc7ead210a2ebff8239dee");
    let new = make("7aa7a5359173d05b63cfd682e3c38487f3cb4f7f1d60659fe59fab1505977d4c");
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"*","b":"*","lib":"1.0.0"}}),
    )]);
    let config = json!({"patchedDependencies":{"lib":"patches/lib.patch"}});
    let patches = BTreeMap::from([("patches/lib.patch".into(), "new\r\n".into())]);
    let normalized_config = cfg(&config);
    let context = MergeContext {
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
    };
    let output = merge(&old, &new, &old, Some(&context)).unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["."],
        new.project().unwrap()["importers"]["."]
    );
    assert_eq!(
        output.project().unwrap()["patchedDependencies"],
        new.project().unwrap()["patchedDependencies"]
    );
    let renamed = json!({"patchedDependencies":{"lib":"patches/renamed.patch"}});
    let contents = BTreeMap::from([("patches/renamed.patch".into(), "new\n".into())]);
    let normalized_renamed = cfg(&renamed);
    let renamed_context = MergeContext {
        registry: None,
        configuration: &normalized_renamed,
        patches: &contents,
        ..context
    };
    let result = merge(&old, &new, &old, Some(&renamed_context)).unwrap();
    assert_eq!(
        result.project().unwrap()["patchedDependencies"]["lib"]["path"],
        "patches/renamed.patch"
    );
    assert_eq!(
        result.project().unwrap()["importers"],
        output.project().unwrap()["importers"]
    );
}

#[test]
fn patched_peer_identity_and_removal_preserve_the_consumer_graph() {
    use sha2::{Digest, Sha256};
    let contents = "peer patch\n";
    let hash = format!("{:x}", Sha256::digest(contents.as_bytes()));
    let provider = format!("1.0.0(patch_hash={hash})");
    let consumer = format!("1.0.0(a@{provider})(b@1.0.0)");
    let mut data = lock(1, 1, false).project().unwrap().clone();
    data["snapshots"].as_object_mut().unwrap().remove("a@1.0.0");
    data["snapshots"]
        .as_object_mut()
        .unwrap()
        .remove("lib@1.0.0(a@1.0.0)(b@1.0.0)");
    data["snapshots"][format!("a@{provider}")] = json!({});
    data["snapshots"][format!("lib@{consumer}")] =
        json!({"dependencies":{"a":provider,"b":"1.0.0"}});
    data["importers"]["."]["dependencies"]["a"]["version"] = json!(provider);
    data["importers"]["."]["dependencies"]["lib"]["version"] = json!(consumer);
    data["patchedDependencies"] = json!({"a":{"path":"a.patch","hash":hash}});
    let input = Lockfile::parse(&data.to_string()).unwrap();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"*","b":"*","lib":"1.0.0"}}),
    )]);
    let config = cfg(&json!({"patchedDependencies":{"a":"a.patch"}}));
    let patches = BTreeMap::from([("a.patch".into(), contents.into())]);
    let output = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &config,
            patches: &patches,
        }),
    )
    .unwrap();
    output.validate().unwrap();
    assert_eq!(output.project(), input.project());

    data["packages"]["wrapper@1.0.0"] = json!({"resolution":{"integrity":"sha512-wrapper"}});
    data["snapshots"]["wrapper@1.0.0"] =
        json!({"dependencies":{"a":provider,"b":"1.0.0","lib":consumer}});
    data["importers"] =
        json!({".":{"dependencies":{"wrapper":{"specifier":"1.0.0","version":"1.0.0"}}}});
    let wrapped = Lockfile::parse(&data.to_string()).unwrap();
    let manifests = BTreeMap::from([(".".into(), json!({"dependencies":{"wrapper":"1.0.0"}}))]);
    let final_config = WorkspaceConfig::default();
    let output = merge(
        &wrapped,
        &wrapped,
        &wrapped,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configurations: [&config; 3],
            configuration: &final_config,
            patches: &BTreeMap::new(),
        }),
    )
    .unwrap();
    output.validate().unwrap();
    let doc = output.project().unwrap();
    assert_eq!(
        doc["snapshots"]["wrapper@1.0.0"]["dependencies"]["a"],
        "1.0.0"
    );
    assert_eq!(
        doc["snapshots"]["wrapper@1.0.0"]["dependencies"]["lib"],
        "1.0.0(a@1.0.0)(b@1.0.0)"
    );
    assert_eq!(
        doc["snapshots"]["lib@1.0.0(a@1.0.0)(b@1.0.0)"]["dependencies"]["a"],
        "1.0.0"
    );
}

#[test]
fn new_and_changed_patches_rebuild_direct_and_transitive_peer_instances() {
    use sha2::{Digest, Sha256};
    for wrapped in [false, true] {
        let mut data = lock(1, 1, false).project().unwrap().clone();
        let manifests = if wrapped {
            data["packages"]["wrapper@1.0.0"] =
                json!({"resolution":{"integrity":"sha512-wrapper"}});
            data["snapshots"]["wrapper@1.0.0"] =
                json!({"dependencies":{"a":"1.0.0","b":"1.0.0","lib":"1.0.0(a@1.0.0)(b@1.0.0)"}});
            data["importers"] =
                json!({".":{"dependencies":{"wrapper":{"specifier":"1.0.0","version":"1.0.0"}}}});
            BTreeMap::from([(".".into(), json!({"dependencies":{"wrapper":"1.0.0"}}))])
        } else {
            BTreeMap::from([(
                ".".into(),
                json!({"dependencies":{"a":"*","b":"*","lib":"1.0.0"}}),
            )])
        };
        let unpatched = Lockfile::parse(&data.to_string()).unwrap();
        let mut input = unpatched.clone();
        let mut before = WorkspaceConfig::default();
        for contents in [Some("first patch\r\n"), Some("updated patch\n"), None] {
            let config = match contents {
                Some(_) => {
                    cfg(&json!({"patchedDependencies":{"a":"a.patch","lib@1.0.0":"lib.patch"}}))
                }
                None => WorkspaceConfig::default(),
            };
            let patches = contents
                .map(|text| {
                    BTreeMap::from([
                        ("a.patch".into(), text.into()),
                        ("lib.patch".into(), text.into()),
                    ])
                })
                .unwrap_or_default();
            let result = merge(
                &input,
                &input,
                &input,
                Some(&MergeContext {
                    registry: None,
                    target: MergeTarget::Ours,
                    manifests: &manifests,
                    configurations: [&before; 3],
                    configuration: &config,
                    patches: &patches,
                }),
            )
            .unwrap();
            result.validate().unwrap();
            let doc = result.project().unwrap();
            let suffix = contents
                .map(|text| {
                    format!(
                        "(patch_hash={:x})",
                        Sha256::digest(text.replace("\r\n", "\n").as_bytes())
                    )
                })
                .unwrap_or_default();
            let provider = format!("1.0.0{suffix}");
            let consumer = format!("1.0.0{suffix}(a@{provider})(b@1.0.0)");
            let dependencies = if wrapped {
                &doc["snapshots"]["wrapper@1.0.0"]["dependencies"]
            } else {
                &doc["importers"]["."]["dependencies"]
            };
            let version = |name: &str| {
                if wrapped {
                    dependencies[name].as_str().unwrap()
                } else {
                    dependencies[name]["version"].as_str().unwrap()
                }
            };
            assert_eq!(version("a"), provider);
            assert_eq!(version("lib"), consumer);
            assert_eq!(
                doc["snapshots"][format!("lib@{consumer}")]["dependencies"]["a"],
                provider
            );
            assert_eq!(doc["packages"], unpatched.project().unwrap()["packages"]);
            if contents.is_none() {
                assert_eq!(doc["snapshots"], unpatched.project().unwrap()["snapshots"]);
                assert_eq!(doc["importers"], unpatched.project().unwrap()["importers"]);
            }
            input = result;
            before = config;
        }
    }
}

#[test]
fn newly_created_patches_follow_exact_range_and_name_selector_precedence() {
    use sha2::{Digest, Sha256};
    let mut data = json!({"lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},"importers":{".":{"dependencies":{}}},"packages":{},"snapshots":{}});
    let mut manifest = json!({"dependencies":{}});
    for major in 1..=3 {
        let name = format!("alias{major}");
        let specifier = format!("npm:a@{major}.0.0");
        let key = format!("a@{major}.0.0");
        data["packages"][&key] = json!({"resolution":{"integrity":format!("sha512-a{major}")}});
        data["snapshots"][&key] = json!({});
        data["importers"]["."]["dependencies"][&name] =
            json!({"specifier":specifier,"version":key});
        manifest["dependencies"][name] = json!(specifier);
    }
    let input = Lockfile::parse(&data.to_string()).unwrap();
    let config = cfg(
        &json!({"patchedDependencies":{"a":"all.patch","a@>=2":"range.patch","a@3.0.0":"exact.patch"}}),
    );
    let patches = BTreeMap::from([
        ("all.patch".into(), "all\n".into()),
        ("range.patch".into(), "range\n".into()),
        ("exact.patch".into(), "exact\n".into()),
    ]);
    let result = merge(
        &input,
        &input,
        &input,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &BTreeMap::from([(".".into(), manifest)]),
            configurations: [&WorkspaceConfig::default(); 3],
            configuration: &config,
            patches: &patches,
        }),
    )
    .unwrap();
    for (major, contents) in [(1, "all\n"), (2, "range\n"), (3, "exact\n")] {
        let hash = format!("{:x}", Sha256::digest(contents.as_bytes()));
        assert_eq!(
            result.project().unwrap()["importers"]["."]["dependencies"][format!("alias{major}")]["version"],
            format!("a@{major}.0.0(patch_hash={hash})")
        );
    }
    result.validate().unwrap();
}

fn transitive_patch_graph(patched: bool) -> serde_json::Value {
    use sha2::{Digest, Sha256};
    let suffix = if patched {
        format!("(patch_hash={:x})", Sha256::digest(b"present\n"))
    } else {
        String::new()
    };
    let leaf = if patched { 2 } else { 1 };
    let mut doc = json!({
        "lockfileVersion":"9.0","settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
        "importers":{".":{"dependencies":{"wrapper":{"specifier":"1.0.0","version":"1.0.0"}}}},
        "packages":{"wrapper@1.0.0":{"resolution":{"integrity":"sha512-wrapper"}},"lib@1.0.0":{"resolution":{"integrity":"sha512-lib"}},format!("leaf@{leaf}.0.0"):{"resolution":{"integrity":format!("sha512-leaf{leaf}")}}},
        "snapshots":{"wrapper@1.0.0":{"dependencies":{"lib":format!("1.0.0{suffix}")}},format!("lib@1.0.0{suffix}"):{"dependencies":{"leaf":format!("{leaf}.0.0")}},format!("leaf@{leaf}.0.0"):{}}
    });
    if patched {
        doc["patchedDependencies"] = json!({"lib@1.0.0":{"hash":format!("{:x}",Sha256::digest(b"present\n")),"path":"lib.patch"}});
    }
    doc
}

#[test]
fn transitive_patch_changes_prefer_an_existing_final_patch_graph() {
    use sha2::{Digest, Sha256};
    let raw = Lockfile::parse(&transitive_patch_graph(false).to_string()).unwrap();
    let patched = Lockfile::parse(&transitive_patch_graph(true).to_string()).unwrap();
    let empty = WorkspaceConfig::default();
    let config = cfg(&json!({"patchedDependencies":{"lib@1.0.0":"lib.patch"}}));
    for (contents, leaf) in [("present\n", 2), ("new\n", 1)] {
        let output = merge(
            &raw,
            &raw,
            &patched,
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Ours,
                manifests: &BTreeMap::from([(
                    ".".into(),
                    json!({"dependencies":{"wrapper":"1.0.0"}}),
                )]),
                configurations: [&empty, &empty, &config],
                configuration: &config,
                patches: &BTreeMap::from([("lib.patch".into(), contents.into())]),
            }),
        )
        .unwrap();
        let key = format!(
            "lib@1.0.0(patch_hash={:x})",
            Sha256::digest(contents.as_bytes())
        );
        assert_eq!(
            output.project().unwrap()["snapshots"][key]["dependencies"]["leaf"],
            format!("{leaf}.0.0")
        );
        output.validate().unwrap();
    }
}

#[test]
fn upgrade_convergence_rehashes_patches_without_updating_their_dependencies() {
    use locksmith::{ConvergeOptions, ManifestChange, converge_with_patches};
    use sha2::{Digest, Sha256};

    let baseline =
        WorkspaceConfig::from_pnpm_version(&json!({}), &"10.33.2".parse().unwrap()).unwrap();
    let config = WorkspaceConfig::from_pnpm_version(
        &json!({"patchedDependencies":{"lib@1.0.0":"lib.patch"}}),
        &"12.21.0".parse().unwrap(),
    )
    .unwrap();
    let hash = format!("{:x}", Sha256::digest(b"present\n"));
    let key = format!("lib@1.0.0(patch_hash={hash})");
    for direct in [false, true] {
        let mut old = transitive_patch_graph(false);
        let mut installed = transitive_patch_graph(true);
        installed["patchedDependencies"]["lib@1.0.0"] = json!(hash);
        let mut manifest = json!({"dependencies":{"wrapper":"1.0.0"}});
        if direct {
            manifest["dependencies"]["lib"] = json!("1.0.0");
            for doc in [&mut old, &mut installed] {
                let pin = doc["snapshots"]["wrapper@1.0.0"]["dependencies"]["lib"].clone();
                doc["importers"]["."]["dependencies"]["lib"] =
                    json!({"specifier":"1.0.0","version":pin});
            }
        }
        let result = converge_with_patches(
            &Lockfile::parse(&old.to_string()).unwrap(),
            &Lockfile::parse(&installed.to_string()).unwrap(),
            &[ManifestChange {
                importer: ".",
                before: Some(&manifest),
                after: Some(&manifest),
            }],
            ConvergeOptions {
                baseline_configuration: Some(&baseline),
                configuration: Some(&config),
                registry: None,
            },
            &BTreeMap::from([("lib.patch".into(), "present\n".into())]),
        )
        .unwrap();
        assert_eq!(
            result.project().unwrap()["snapshots"][&key]["dependencies"]["leaf"],
            "1.0.0"
        );
    }
}

#[test]
fn patch_selectors_use_the_release_version_recorded_for_a_tarball() {
    use sha2::{Digest, Sha256};
    let url = "https://example.org/project.tgz";
    let hash = format!("{:x}", Sha256::digest(b"patch\n"));
    let reference = format!("{url}(patch_hash={hash})");
    let doc = json!({
        "lockfileVersion":"9.0","patchedDependencies":{"project@1.2.0":hash},
        "importers":{".":{"devDependencies":{"project":{"specifier":url,"version":reference}}}},
        "packages":{format!("project@{url}"):{"resolution":{"tarball":url},"version":"1.2.0"}},
        "snapshots":{format!("project@{reference}"):{}}
    });
    let lock = Lockfile::parse(&doc.to_string()).unwrap();
    let manifests = BTreeMap::from([(".".into(), json!({"devDependencies":{"project":url}}))]);
    let mut config = cfg(&json!({"patchedDependencies":{"project@1.2.0":"project.patch"}}));
    config.patch_encoding = locksmith::PatchEncoding::Sha256;
    let patches = BTreeMap::from([("project.patch".into(), "patch\n".into())]);
    let output = merge(
        &lock,
        &lock,
        &lock,
        Some(&MergeContext {
            registry: None,
            target: MergeTarget::Ours,
            manifests: &manifests,
            configuration: &config,
            configurations: [&config; 3],
            patches: &patches,
        }),
    )
    .unwrap();
    assert_eq!(
        output.project().unwrap()["importers"]["."]["devDependencies"]["project"]["version"],
        reference
    );
}
