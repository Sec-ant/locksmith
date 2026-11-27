use super::{cfg, lock};
use locksmith::{
    Lockfile, LockfileError, MergeContext, MergeTarget, PeerRules, PeerVersionMatching, Value,
    WorkspaceConfig, inspect_peers, merge,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn peer_diagnostics_do_not_decide_acceptance_or_change_the_graph() {
    let input = |a| {
        let mut data = lock(a, 1, false).project().unwrap().clone();
        data["packages"]["lib@1.0.0"]["peerDependencies"]["a"] = json!("^1");
        Lockfile::parse(&data.to_string()).unwrap()
    };
    let base = input(1);
    let changed = input(2);
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"2.0.0","b":"*","lib":"1.0.0"}}),
    )]);
    let config = json!({});
    let strict = json!({"strictPeerDependencies":true});
    let patches = BTreeMap::new();
    let normalized_config = cfg(&config);
    let mut context = MergeContext {
        registry: None,
        target: MergeTarget::Theirs,
        manifests: &manifests,
        configurations: [&normalized_config; 3],
        configuration: &normalized_config,
        patches: &patches,
    };
    let output = merge(&base, &changed, &base, Some(&context)).unwrap();
    let issues = inspect_peers(&output, &PeerRules::default(), None);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].peer, "a");
    assert_eq!(issues[0].resolved.as_deref(), Some("2.0.0"));
    let strict_config = cfg(&strict);
    context.configuration = &strict_config;
    let same = merge(&base, &changed, &base, Some(&context)).unwrap();
    assert_eq!(same.project(), output.project());
    assert_eq!(inspect_peers(&same, &PeerRules::default(), None), issues);
}

#[test]
fn configured_peer_exceptions_apply_before_strict_policy() {
    let mut data = lock(2, 1, false).project().unwrap().clone();
    data["packages"]["lib@1.0.0"]["peerDependencies"]["a"] = json!("^1");
    data["packages"]["lib@1.0.0"]["peerDependencies"]["@local/optional"] = json!("*");
    let input = Lockfile::parse(&data.to_string()).unwrap();
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"dependencies":{"a":"2.0.0","b":"*","lib":"1.0.0"}}),
    )]);
    let before = json!({});
    let patches = BTreeMap::new();
    for rules in [
        json!({"allowedVersions":{"lib@1>a":"^2"},"ignoreMissing":["@local/*"]}),
        json!({"allowAny":["*","!b"],"ignoreMissing":["@local/*"]}),
    ] {
        let config = json!({"strictPeerDependencies":true,"peerDependencyRules":rules});
        let output = merge(
            &input,
            &input,
            &input,
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&cfg(&before); 3],
                configuration: &cfg(&config),
                patches: &patches,
            }),
        )
        .unwrap();
        assert!(
            inspect_peers(
                &output,
                &PeerRules::from_pnpm(&config, PeerVersionMatching::Semver).unwrap(),
                None
            )
            .is_empty()
        );
    }
}

#[test]
fn malformed_context_mappings_return_structured_errors() {
    let input = lock(1, 1, false);
    let manifests = BTreeMap::from([(".".into(), json!({}))]);
    let patches = BTreeMap::new();
    let config = json!({"catalogs":false,"catalog":{}});
    assert!(matches!(
        WorkspaceConfig::from_pnpm(&config),
        Err(LockfileError::InvalidDocument(_))
    ));
    let mut value = input.project().unwrap().clone();
    value["settings"] = json!(false);
    let invalid = Lockfile::parse(&value.to_string()).unwrap();
    let config = json!({});
    assert!(
        matches!(merge(&invalid,&invalid,&invalid,Some(&MergeContext{registry:None,target:MergeTarget::Theirs,manifests:&manifests,configurations:[&cfg(&config);3],configuration:&cfg(&config),patches:&patches})),Err(LockfileError::ResolutionRequired{path,..}) if path==["settings"])
    );
}

#[test]
fn strict_peer_policy_uses_pnpm_prerelease_matching() {
    let mut data: Value = serde_json::from_str(
        &lock(1, 1, false)
            .project()
            .unwrap()
            .to_string()
            .replace("a@1.0.0", "a@1.0.0-beta.2"),
    )
    .unwrap();
    data["importers"]["."]["dependencies"]["a"] =
        json!({"specifier":"1.0.0-beta.2","version":"1.0.0-beta.2"});
    data["snapshots"]["lib@1.0.0(a@1.0.0-beta.2)(b@1.0.0)"]["dependencies"]["a"] =
        json!("1.0.0-beta.2");
    let manifests = BTreeMap::from([(
        ".".into(),
        json!({"packageManager":"pnpm@10.33.2","dependencies":{"a":"1.0.0-beta.2","b":"*","lib":"1.0.0"}}),
    )]);
    let config = json!({"strictPeerDependencies":true});
    let patches = BTreeMap::new();
    for (range, accepted) in [
        ("^1", true),
        ("1.0.0-rc.1", false),
        ("<1.0.0", true),
        ("^2", false),
    ] {
        data["packages"]["lib@1.0.0"]["peerDependencies"]["a"] = json!(range);
        let input = Lockfile::parse(&data.to_string()).unwrap();
        let result = merge(
            &input,
            &input,
            &input,
            Some(&MergeContext {
                registry: None,
                target: MergeTarget::Theirs,
                manifests: &manifests,
                configurations: [&cfg(&config); 3],
                configuration: &cfg(&config),
                patches: &patches,
            }),
        );
        let output = result.unwrap();
        let rules = PeerRules {
            version_matching: PeerVersionMatching::ReleaseCompatible,
            ..PeerRules::default()
        };
        assert_eq!(
            inspect_peers(&output, &rules, None).is_empty(),
            accepted,
            "{range}"
        );
    }
}

#[test]
fn contextual_merge_requires_recorded_settings_matching_the_final_workspace() {
    let pnpm = "12.15.0".parse().unwrap();
    let config = |value| WorkspaceConfig::from_pnpm_version(&value, &pnpm).unwrap();
    let opts = |auto_dedupe, dedupe_peer_dependents| {
        json!({
            "lockfile": {"includeResolutionSettings": true},
            "autoDedupe": auto_dedupe,
            "dedupePeerDependents": dedupe_peer_dependents,
        })
    };
    let input = |auto_dedupe, dedupe_peer_dependents| {
        let document = json!({"lockfileVersion": "9.0", "importers": {".": {}}, "settings": {
            "autoDedupe": auto_dedupe,
            "dedupeInjectedDeps": true,
            "dedupePeerDependents": dedupe_peer_dependents,
            "linkWorkspacePackages": false,
        }});
        Lockfile::parse(&document.to_string()).unwrap()
    };
    let configurations = [
        config(opts(false, true)),
        config(opts(true, true)),
        config(opts(false, false)),
    ];
    let [base, ours, theirs] = [input(false, true), input(true, true), input(false, false)];
    let manifests = BTreeMap::from([(".".into(), json!({}))]);
    let final_config = config(opts(true, false));
    let mut context = MergeContext {
        registry: None,
        target: MergeTarget::Ours,
        manifests: &manifests,
        configurations: configurations.each_ref(),
        configuration: &final_config,
        patches: &BTreeMap::new(),
    };
    assert!(matches!(
        merge(&base, &ours, &theirs, Some(&context)),
        Err(LockfileError::ResolutionRequired { path, .. }) if path == ["settings", "autoDedupe"]
    ));

    context.configuration = &configurations[2];
    let merged = merge(&base, &ours, &theirs, Some(&context)).unwrap();
    let settings = &merged.project().unwrap()["settings"];
    assert_eq!(settings["autoDedupe"], false);
    assert_eq!(settings["dedupePeerDependents"], false);

    context.target = MergeTarget::Theirs;
    context.configurations = [&configurations[0], &configurations[1], &configurations[0]];
    context.configuration = &configurations[1];
    let merged = merge(&base, &ours, &base, Some(&context)).unwrap();
    assert_eq!(merged.project().unwrap()["settings"]["autoDedupe"], true);
}

#[test]
fn configuration_decoding_normalizes_defaults_and_excludes_acceptance_policy() {
    let implicit = WorkspaceConfig::from_pnpm(&json!({})).unwrap();
    let explicit=WorkspaceConfig::from_pnpm(&json!({"autoInstallPeers":true,"dedupePeers":false,"excludeLinksFromLockfile":false,"resolvePeersFromWorkspaceRoot":true,"injectWorkspacePackages":false,"strictPeerDependencies":true})).unwrap();
    assert_eq!(implicit, explicit);
    assert!(WorkspaceConfig::from_pnpm(&json!({"autoInstallPeers":"false"})).is_err());
    assert!(WorkspaceConfig::from_pnpm(&json!({"dedupePeers":"false"})).is_err());
}
