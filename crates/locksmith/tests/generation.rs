//! Observable contracts of generation, independent of an install or registry server.
use locksmith::{
    GenerateContext, Lockfile, LockfileError, PackageSource, RegistryPackage, SourceManifest,
    Value, generate,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
type Result<T> = std::result::Result<T, LockfileError>;

#[derive(Default)]
struct Registry {
    packages: BTreeMap<String, BTreeMap<String, Value>>,
    tarballs: BTreeSet<String>,
    cached_tarballs: BTreeSet<String>,
    tags: BTreeMap<(&'static str, &'static str), &'static str>,
}
impl Registry {
    fn add(&mut self, name: &str, version: &str, manifest: Value) {
        self.packages
            .entry(name.into())
            .or_default()
            .insert(version.into(), manifest);
    }
}
impl PackageSource for Registry {
    fn registry_selector(
        &mut self,
        name: &str,
        specifier: &str,
    ) -> Result<Option<(RegistryPackage, String)>> {
        Ok(match specifier.strip_prefix("mirror:") {
            Some(selector) => {
                RegistryPackage::selector(name, selector).map(|(mut package, range)| {
                    package.prefix = Some("mirror".into());
                    (package, range)
                })
            }
            None => RegistryPackage::selector(name, specifier),
        })
    }

    fn versions(
        &mut self,
        package: &RegistryPackage,
        selector: &str,
        _: bool,
    ) -> Result<Vec<String>> {
        let Ok(range) = selector.parse::<node_semver::Range>() else {
            return Ok(self
                .tags
                .get(&(package.name.as_str(), selector))
                .map(|v| v.to_string())
                .into_iter()
                .collect());
        };
        let mut versions: Vec<_> = self
            .packages
            .get(&package.name)
            .into_iter()
            .flat_map(|m| m.keys())
            .map(|s| s.parse::<node_semver::Version>().unwrap())
            .filter(|v| v.satisfies(&range))
            .collect();
        versions.sort_by(|a, b| b.cmp(a));
        Ok(versions.into_iter().map(|v| v.to_string()).collect())
    }
    fn release(&mut self, package: &RegistryPackage, version: &str) -> Result<SourceManifest> {
        let mut raw = self.packages[&package.name][version].clone();
        raw["name"] = package.name.clone().into();
        raw["version"] = version.into();
        SourceManifest::new(
            package.key(version),
            raw,
            json!({"integrity":"sha512-fixture"}),
        )
    }
    fn other(&mut self, _: &str, name: &str, specifier: &str) -> Result<SourceManifest> {
        assert!(
            self.tarballs.contains(specifier),
            "unexpected fetch for {specifier}"
        );
        self.cached_tarballs.insert(specifier.into());
        SourceManifest::new(
            format!("{name}@{specifier}"),
            json!({"name":name,"version":"1.0.0"}),
            json!({"integrity":"sha512-fixture","tarball":specifier}),
        )
    }
    fn locked_source(
        &mut self,
        name: &str,
        locator: &str,
        resolution: &Value,
    ) -> Result<SourceManifest> {
        assert!(
            self.cached_tarballs.contains(locator),
            "missing cached archive {locator}"
        );
        SourceManifest::new(
            format!("{name}@{locator}"),
            json!({"name":name,"version":"1.0.0"}),
            resolution.clone(),
        )
    }
}
fn workspace(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(id, v)| (id.to_string(), v.clone()))
        .collect()
}

#[test]
fn cyclic_peers_keep_the_providers_each_workspace_actually_supplies() {
    let mut registry = Registry::default();
    registry.add("ui", "1.0.0", json!({"peerDependencies":{"runner":"1"}}));
    registry.add(
        "runner",
        "1.0.0",
        json!({
            "peerDependencies":{"ui":"1","extension":"*"},
            "peerDependenciesMeta":{"extension":{"optional":true}}
        }),
    );
    for v in ["1.0.0", "2.0.0"] {
        registry.add("extension", v, json!({}));
    }
    let app = |v: &str| json!({"dependencies":{"runner":"1","ui":"1","extension":v}});
    let manifests = workspace(&[
        (".", json!({})),
        ("one", app("1.0.0")),
        ("two", app("2.0.0")),
    ]);
    for flat in [false, true] {
        let config = json!({"dedupePeers":flat,"autoInstallPeers":false});
        let lock = run(
            &empty_workspace_lock(),
            &manifests,
            config.clone(),
            &mut registry,
        )
        .unwrap();
        let doc = lock.project().unwrap();
        for (id, expected) in [("one", "1.0.0"), ("two", "2.0.0")] {
            let root = &doc["importers"][id]["dependencies"];
            let ui = format!("ui@{}", root["ui"]["version"].as_str().unwrap());
            let pin = &doc["snapshots"][ui]["dependencies"]["runner"];
            assert_eq!(pin, &root["runner"]["version"]);
            let runner = format!("runner@{}", pin.as_str().unwrap());
            assert_eq!(
                doc["snapshots"][runner]["optionalDependencies"]["extension"],
                expected
            );
        }
        let repeated = run(&lock, &manifests, config, &mut registry).unwrap();
        assert_eq!(repeated.to_yaml().unwrap(), lock.to_yaml().unwrap());
    }
}
fn empty_workspace_lock() -> Lockfile {
    Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}}\n").unwrap()
}

fn run(
    old: &Lockfile,
    inputs: &BTreeMap<String, Value>,
    config: Value,
    source: &mut Registry,
) -> Result<Lockfile> {
    generate(
        old,
        &GenerateContext::new(inputs, &config)
            .with_pnpm_version(&node_semver::Version::from((12, 21, 0))),
        source,
    )
}

#[test]
fn a_new_member_inherits_the_updated_root_peer_instance() {
    let mut registry = Registry::default();
    for version in ["1.0.0", "2.0.0"] {
        registry.add("host", version, json!({}));
    }
    registry.add("bridge", "1.0.0", json!({"peerDependencies":{"host":"*"}}));
    registry.add(
        "consumer",
        "1.0.0",
        json!({"peerDependencies":{"bridge":"1"}}),
    );
    let member = json!({"dependencies":{"consumer":"1"}});
    let mut inputs = workspace(&[
        (".", json!({"dependencies":{"bridge":"1","host":"1"}})),
        ("existing", member.clone()),
    ]);
    let config = json!({"autoInstallPeers":false});
    let old = run(
        &empty_workspace_lock(),
        &inputs,
        config.clone(),
        &mut registry,
    )
    .unwrap();
    inputs.get_mut(".").unwrap()["dependencies"]["host"] = "2".into();
    inputs.insert("-new".into(), member);
    inputs.insert(
        "shadowed".into(),
        json!({"dependencies":{"consumer":"1","bridge":"1","host":"1"}}),
    );
    let next = run(&old, &inputs, config, &mut registry).unwrap();
    let importers = &next.project().unwrap()["importers"];
    for id in ["existing", "-new"] {
        assert_eq!(
            importers[id]["dependencies"]["consumer"]["version"], "1.0.0(bridge@1.0.0(host@2.0.0))",
            "{id}"
        );
    }
    assert_eq!(
        importers["shadowed"]["dependencies"]["consumer"]["version"],
        "1.0.0(bridge@1.0.0(host@1.0.0))"
    );
}
#[test]
fn changing_a_transitive_override_preserves_independent_peer_instances() {
    let manifests = workspace(&[(
        ".",
        json!({"dependencies":{"parent":"1","plugin":"1","host":"1"}}),
    )]);
    let mut original = Registry::default();
    original.add("parent", "1.0.0", json!({"dependencies":{"leaf":"*"}}));
    original.add("leaf", "1.0.0", json!({}));
    original.add("plugin", "1.0.0", json!({"peerDependencies":{"host":"1"}}));
    original.add("host", "1.0.0", json!({}));
    let old = run(
        &empty_workspace_lock(),
        &manifests,
        json!({"overrides":{"leaf":"1"}}),
        &mut original,
    )
    .unwrap();

    let mut available = Registry::default();
    available.add("parent", "1.0.0", json!({"dependencies":{"leaf":"*"}}));
    available.add("leaf", "2.0.0", json!({}));
    let updated = run(
        &old,
        &manifests,
        json!({"overrides":{"leaf":"2"}}),
        &mut available,
    )
    .unwrap();
    let doc = updated.project().unwrap();
    assert_eq!(
        doc["snapshots"]["parent@1.0.0"]["dependencies"]["leaf"],
        "2.0.0"
    );
    assert!(doc["packages"].get("leaf@1.0.0").is_none());
    let plugin = &doc["importers"]["."]["dependencies"]["plugin"]["version"];
    assert_eq!(
        plugin,
        &old.project().unwrap()["importers"]["."]["dependencies"]["plugin"]["version"]
    );
    let key = format!("plugin@{}", plugin.as_str().unwrap());
    assert_eq!(doc["snapshots"][key]["dependencies"]["host"], "1.0.0");
}

#[test]
fn a_release_keeps_its_recorded_choice_between_own_dependency_and_peer() {
    let mut registry = Registry::default();
    registry.add("child", "1.0.0", json!({}));
    registry.add(
        "plugin",
        "1.0.0",
        json!({"dependencies":{"child":"1"},"peerDependencies":{"child":"1"}}),
    );
    // A pnpmfile host re-evaluates every manifest, so locked releases are
    // rebuilt from their publications instead of their historical records.
    struct Unchanged;
    impl locksmith::GenerationHooks for Unchanged {
        fn read_package(&mut self, _: &str, raw: Value) -> Result<Value> {
            Ok(raw)
        }
    }
    let version = "10.12.4".parse().unwrap();
    let generate_with = |old: &Lockfile, root: Value, registry: &mut Registry| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(
                &workspace(&[(".", root)]),
                &json!({"autoInstallPeers":false}),
            )
            .with_pnpm_version(&version),
            registry,
            &mut Unchanged,
        )
        .unwrap()
    };
    let old = generate_with(
        &empty_workspace_lock(),
        json!({"dependencies":{"plugin":"1"}}),
        &mut registry,
    );
    let before = old.project().unwrap();
    assert!(
        before["packages"]["plugin@1.0.0"]
            .get("peerDependencies")
            .is_none()
    );
    // The project now supplies the peer. The locked release already records
    // that it uses its own dependency, and the update keeps that instance.
    let updated = generate_with(
        &old,
        json!({"dependencies":{"plugin":"1","child":"1"}}),
        &mut registry,
    );
    let doc = updated.project().unwrap();
    assert!(
        doc["packages"]["plugin@1.0.0"]
            .get("peerDependencies")
            .is_none()
    );
    assert_eq!(
        doc["importers"]["."]["dependencies"]["plugin"]["version"],
        before["importers"]["."]["dependencies"]["plugin"]["version"]
    );
}

#[test]
fn overlapping_disjunctions_share_the_minimum_number_of_releases() {
    let mut registry = Registry::default();
    for n in 1..=6 {
        registry.add("lib", &format!("{n}.0.0"), json!({}));
    }
    let manifests = workspace(&[(
        ".",
        json!({"dependencies":{"a":"npm:lib@1 || 2 || 6","b":"npm:lib@2 || 3","c":"npm:lib@3 || 4","d":"npm:lib@4 || 5","e":"npm:lib@2 || 4"}}),
    )]);
    let lock = run(
        &empty_workspace_lock(),
        &manifests,
        json!({}),
        &mut registry,
    )
    .unwrap();
    let names: BTreeSet<_> = lock.dependency_closure(".").unwrap().packages;
    // b/c and a/d require at least two releases; only versions 2 and 4 cover all five ranges together.
    assert_eq!(
        names,
        BTreeSet::from(["lib@2.0.0".into(), "lib@4.0.0".into()])
    );
}

#[test]
fn a_transitive_selector_without_an_eligible_release_reopens_the_parent_choice() {
    let mut registry = Registry::default();
    registry.add("tool", "1.1.0", json!({"dependencies":{"obsolete":"2"}}));
    registry.add("tool", "1.0.0", json!({"dependencies":{"available":"1"}}));
    registry.add("available", "1.0.0", json!({}));
    let manifests = workspace(&[(".", json!({"dependencies":{"tool":"^1"}}))]);
    let lock = run(
        &empty_workspace_lock(),
        &manifests,
        json!({}),
        &mut registry,
    )
    .unwrap();
    assert_eq!(
        lock.dependency_closure(".").unwrap().packages,
        BTreeSet::from(["tool@1.0.0".into(), "available@1.0.0".into()])
    );
}

#[test]
fn an_upstream_release_requiring_an_absent_workspace_member_can_be_replaced() {
    let mut registry = Registry::default();
    registry.add(
        "tool",
        "1.1.0",
        json!({"dependencies":{"private-plugin":"workspace:*"}}),
    );
    registry.add("tool", "1.0.0", json!({}));
    let manifests = workspace(&[(".", json!({"dependencies":{"tool":"^1"}}))]);
    let config = json!({});
    for exhaustive in [false, true] {
        let input = GenerateContext::new(&manifests, &config).with_exhaustive(exhaustive);
        let lock = generate(&empty_workspace_lock(), &input, &mut registry).unwrap();
        assert_eq!(
            lock.dependency_closure(".").unwrap().packages,
            BTreeSet::from(["tool@1.0.0".into()])
        );
    }
}

#[test]
fn refreshed_registry_metadata_preserves_a_matching_historical_sha1_digest() {
    let mut registry = Registry::default();
    registry.add(
        "plugin",
        "1.0.0",
        json!({"dist":{"shasum":"da39a3ee5e6b4b0d3255bfef95601890afd80709"}}),
    );
    let inputs = workspace(&[(".", json!({"dependencies":{"plugin":"1.0.0"}}))]);
    let old = run(&empty_workspace_lock(), &inputs, json!({}), &mut registry)
        .unwrap()
        .to_yaml()
        .unwrap();
    let config = json!({"packageExtensions":{"plugin@1":{"engines":{"node":">=18"}}}});
    let digest = "sha1-2jmj7l5rSw0yVb/vlWAYkK/YBwk=";
    let prior = Lockfile::parse(&old.replace("sha512-fixture", digest)).unwrap();
    let refreshed = run(&prior, &inputs, config.clone(), &mut registry).unwrap();
    assert_eq!(
        refreshed.project().unwrap()["packages"]["plugin@1.0.0"]["resolution"]["integrity"],
        digest
    );
    let wrong =
        Lockfile::parse(&old.replace("sha512-fixture", "sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA="))
            .unwrap();
    assert!(run(&wrong, &inputs, config, &mut registry).is_err());
}

#[test]
fn editing_a_sibling_workspace_preserves_an_existing_unresolved_transitive_workspace_peer() {
    let old = Lockfile::parse(
        "lockfileVersion: '9.0'\nsettings: {autoInstallPeers: true, excludeLinksFromLockfile: false}\nimporters:\n  .: {}\n  editor: {}\n  legacy:\n    dependencies:\n      legacy-plugin: {specifier: 1.0.0, version: 1.0.0}\n      legacy: {specifier: 'workspace:*', version: 'link:'}\npackages:\n  legacy-plugin@1.0.0:\n    resolution: {integrity: sha512-fixture}\n    peerDependencies: {workspace-codec: 'workspace:>='}\nsnapshots:\n  legacy-plugin@1.0.0: {}\n",
    ).unwrap();
    let inputs = workspace(&[
        (".", json!({})),
        (
            "legacy",
            json!({"name":"legacy","version":"1.0.0","dependencies":{"legacy-plugin":"1.0.0","legacy":"workspace:*"}}),
        ),
        ("editor", json!({"dependencies":{"fresh":"1.0.0"}})),
    ]);
    let mut registry = Registry::default();
    registry.add("fresh", "1.0.0", json!({}));
    let updated = run(&old, &inputs, json!({"dedupePeers":false}), &mut registry).unwrap();
    assert_eq!(
        updated.project().unwrap()["importers"]["legacy"],
        old.project().unwrap()["importers"]["legacy"]
    );
    assert_eq!(
        updated.dependency_closure("legacy").unwrap(),
        old.dependency_closure("legacy").unwrap()
    );
    assert_eq!(
        updated.project().unwrap()["importers"]["editor"]["dependencies"]["fresh"]["version"],
        "1.0.0"
    );
}
#[test]
fn optional_peer_introduced_in_one_consumer_splits_the_transitive_snapshot() {
    let mut registry = Registry::default();
    registry.add(
        "wrapper",
        "1.0.0",
        json!({"dependencies":{"plugin":"1.0.0"}}),
    );
    registry.add("plugin","1.0.0",json!({"peerDependencies":{"provider":"^2"},"peerDependenciesMeta":{"provider":{"optional":true}}}));
    registry.add("provider", "2.0.0", json!({}));
    let mut inputs = workspace(&[
        (".", json!({})),
        ("apps/a", json!({"dependencies":{"wrapper":"1.0.0"}})),
        ("apps/b", json!({"dependencies":{"wrapper":"1.0.0"}})),
    ]);
    let old = run(&empty_workspace_lock(), &inputs, json!({}), &mut registry).unwrap();
    inputs.get_mut("apps/a").unwrap()["dependencies"]["provider"] = "2.0.0".into();
    let lock = run(&old, &inputs, json!({}), &mut registry).unwrap();
    let a = lock.dependency_closure("apps/a").unwrap();
    assert!(a.snapshots.contains("plugin@1.0.0(provider@2.0.0)"));
    assert!(a.snapshots.contains("wrapper@1.0.0(provider@2.0.0)"));
    assert_eq!(
        lock.dependency_closure("apps/b").unwrap(),
        old.dependency_closure("apps/b").unwrap()
    );
    assert_eq!(
        lock.project().unwrap()["snapshots"]["plugin@1.0.0(provider@2.0.0)"]["optionalDependencies"]
            ["provider"],
        "2.0.0"
    );
    let yaml = lock.to_yaml().unwrap();
    assert_eq!(
        run(&lock, &inputs, json!({}), &mut Registry::default())
            .unwrap()
            .to_yaml()
            .unwrap(),
        yaml
    );
    let v13 = lock.to_pnpm_v13().unwrap();
    assert_eq!(
        Lockfile::parse_generation_input(v13).unwrap().project(),
        lock.project()
    );
}

#[test]
fn the_v13_format_keeps_an_ordinary_edge_for_an_unbound_peer() {
    // The plugin installs its own `child` and also declares it as a peer that
    // the app does not supply.
    let mut registry = Registry::default();
    registry.add(
        "plugin",
        "1.0.0",
        json!({"dependencies":{"child":"1"},"peerDependencies":{"child":"1"}}),
    );
    registry.add("child", "1.0.0", json!({}));
    let inputs = workspace(&[(".", json!({"dependencies":{"plugin":"1"}}))]);
    let lock = run(
        &empty_workspace_lock(),
        &inputs,
        json!({"autoInstallPeers":false}),
        &mut registry,
    )
    .unwrap();
    let v13 = lock.to_pnpm_v13().unwrap();
    assert_eq!(
        Lockfile::parse_generation_input(v13).unwrap().project(),
        lock.project()
    );
}
#[test]
fn strict_peers_can_change_the_declaring_release_with_or_without_an_incumbent() {
    let mut registry = Registry::default();
    registry.add(
        "consumer",
        "1.0.0",
        json!({"peerDependencies":{"provider":"^1"}}),
    );
    registry.add(
        "consumer",
        "1.1.0",
        json!({"peerDependencies":{"provider":"^2"}}),
    );
    for v in ["1.0.0", "2.0.0"] {
        registry.add("provider", v, json!({}));
    }
    let mut inputs = workspace(&[(
        ".",
        json!({"dependencies":{"consumer":"^1","provider":"2.0.0"}}),
    )]);
    let cfg = json!({"strictPeerDependencies":true});
    let old = run(&empty_workspace_lock(), &inputs, cfg.clone(), &mut registry).unwrap();
    inputs.get_mut(".").unwrap()["dependencies"]["provider"] = "1.0.0".into();
    for previous in [&empty_workspace_lock(), &old] {
        let lock = run(previous, &inputs, cfg.clone(), &mut registry).unwrap();
        assert_eq!(
            lock.project().unwrap()["importers"]["."]["dependencies"]["consumer"]["version"],
            "1.0.0(provider@1.0.0)"
        );
    }
}

#[test]
fn a_workspace_can_change_a_grandparent_to_share_a_transitive_release() {
    let mut registry = Registry::default();
    for v in ["1.0.0", "2.0.0"] {
        registry.add("leaf", v, json!({}));
        registry.add("bridge", v, json!({"dependencies":{"leaf":v}}));
        registry.add("left", v, json!({"dependencies":{"bridge":v}}));
    }
    registry.add("right", "1.0.0", json!({"dependencies":{"leaf":"1.0.0"}}));
    let manifests = workspace(&[(".", json!({"dependencies":{"left":"*","right":"1.0.0"}}))]);
    for exhaustive in [false, true] {
        let lock = generate(
            &empty_workspace_lock(),
            &GenerateContext::new(&manifests, &json!({}))
                .with_pnpm_version(&node_semver::Version::from((10, 34, 9)))
                .with_exhaustive(exhaustive),
            &mut registry,
        )
        .unwrap();
        assert_eq!(
            lock.dependency_closure(".").unwrap().packages,
            BTreeSet::from([
                "left@1.0.0".into(),
                "right@1.0.0".into(),
                "bridge@1.0.0".into(),
                "leaf@1.0.0".into()
            ])
        );
    }
}

#[test]
fn an_upgrade_can_reuse_a_locked_leaf_through_a_previously_unseen_release() {
    let mut registry = Registry::default();
    for version in ["1.0.0", "2.0.0"] {
        registry.add("leaf", version, json!({}));
    }
    registry.add("host", "1.0.0", json!({"dependencies":{"leaf":"1"}}));
    let original = workspace(&[(".", json!({"dependencies":{"host":"1"}}))]);
    let previous = run(&empty_workspace_lock(), &original, json!({}), &mut registry).unwrap();
    for (version, bridge, leaf) in [
        ("1.1.0", "1.0.0", "1"),
        ("1.2.0", "2.0.0", "2"),
        ("1.3.0", "3.0.0", "2"),
    ] {
        registry.add("plugin", version, json!({"dependencies":{"bridge":bridge}}));
        registry.add("bridge", bridge, json!({"dependencies":{"leaf":leaf}}));
    }
    let next = workspace(&[(".", json!({"dependencies":{"host":"1","plugin":"^1.1.0"}}))]);
    let lock = run(&previous, &next, json!({}), &mut registry).unwrap();
    let packages = lock.dependency_closure(".").unwrap().packages;
    assert!(packages.contains("leaf@1.0.0"));
    assert!(!packages.contains("leaf@2.0.0"));
}

#[test]
fn an_upgraded_plugin_can_reuse_a_dependency_pinned_by_an_unchanged_project() {
    let mut registry = Registry::default();
    for n in 1..=3 {
        registry.add("engine", &format!("{n}.0.0"), json!({}));
        registry.add(
            "plugin",
            &format!("1.{n}.0"),
            json!({"dependencies":{"engine":format!("{n}.0.0")}}),
        );
    }
    let mut manifests = workspace(&[
        (".", json!({"devDependencies":{"plugin":"1.1.0"}})),
        ("client", json!({"dependencies":{"engine":"2.0.0"}})),
    ]);
    let old = run(
        &empty_workspace_lock(),
        &manifests,
        json!({}),
        &mut registry,
    )
    .unwrap();
    manifests.get_mut(".").unwrap()["devDependencies"]["plugin"] = "^1.2.0".into();
    let next = run(&old, &manifests, json!({}), &mut registry).unwrap();
    assert_eq!(
        next.dependency_closure(".").unwrap().packages,
        BTreeSet::from(["plugin@1.2.0".into(), "engine@2.0.0".into()])
    );
    assert_eq!(
        next.dependency_closure("client").unwrap(),
        old.dependency_closure("client").unwrap()
    );
}

#[test]
fn a_host_upgrade_preserves_the_locked_plugin_and_adapter_releases() {
    let mut registry = Registry::default();
    registry.add("host", "1.0.0", json!({}));
    registry.add("host", "2.0.0", json!({}));
    registry.add("plugin", "1.0.0", json!({"peerDependencies":{"host":"1"}}));
    registry.add(
        "adapter",
        "1.0.0",
        json!({"peerDependencies":{"plugin":"1.0.0"}}),
    );
    let mut manifests = workspace(&[(
        ".",
        json!({"dependencies":{"host":"1.0.0","plugin":"^1","adapter":"^1"}}),
    )]);
    let cfg = json!({"autoInstallPeers":false});
    let old = run(
        &empty_workspace_lock(),
        &manifests,
        cfg.clone(),
        &mut registry,
    )
    .unwrap();
    registry.add("plugin", "1.1.0", json!({"peerDependencies":{"host":"2"}}));
    registry.add(
        "adapter",
        "1.1.0",
        json!({"peerDependencies":{"plugin":"1.1.0"}}),
    );
    manifests.get_mut(".").unwrap()["dependencies"]["host"] = "2.0.0".into();
    let next = run(&old, &manifests, cfg, &mut registry).unwrap();
    assert_eq!(
        next.dependency_closure(".").unwrap().packages,
        BTreeSet::from([
            "adapter@1.0.0".into(),
            "plugin@1.0.0".into(),
            "host@2.0.0".into()
        ])
    );
}

#[test]
fn removing_a_peer_in_a_hook_collapses_instances_and_keeping_it_preserves_the_provider() {
    use locksmith::{GenerationHooks, generate_with_hooks};
    struct MovePeer {
        keep_peer: bool,
    }
    impl GenerationHooks for MovePeer {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "plugin" {
                raw["dependencies"] = raw["peerDependencies"].clone();
                if !self.keep_peer {
                    raw.as_object_mut().unwrap().remove("peerDependencies");
                }
            }
            Ok(raw)
        }
    }
    let mut registry = Registry::default();
    for v in ["1.0.0", "1.1.0"] {
        registry.add("engine", v, json!({}));
    }
    registry.add(
        "plugin",
        "1.0.0",
        json!({"peerDependencies":{"engine":"^1"}}),
    );
    let app = |v: &str| json!({"dependencies":{"plugin":"1.0.0","engine":v}});
    let manifests = workspace(&[
        (".", json!({})),
        ("one", app("1.0.0")),
        ("two", app("1.1.0")),
    ]);
    let cfg = json!({"autoInstallPeers":false});
    let old = run(
        &empty_workspace_lock(),
        &manifests,
        cfg.clone(),
        &mut registry,
    )
    .unwrap();
    for (exhaustive, keep_peer) in [(false, false), (false, true), (true, false), (true, true)] {
        let input = GenerateContext::new(&manifests, &cfg).with_exhaustive(exhaustive);
        let lock =
            generate_with_hooks(&old, &input, &mut registry, &mut MovePeer { keep_peer }).unwrap();
        let doc = lock.project().unwrap();
        for (id, v) in [("one", "1.0.0"), ("two", "1.1.0")] {
            assert_eq!(doc["importers"][id]["dependencies"]["engine"]["version"], v);
            let expected = if keep_peer {
                format!("1.0.0(engine@{v})")
            } else {
                "1.0.0".into()
            };
            assert_eq!(
                doc["importers"][id]["dependencies"]["plugin"]["version"],
                expected
            );
            if keep_peer {
                assert_eq!(
                    doc["snapshots"][format!("plugin@{expected}")]["dependencies"]["engine"],
                    v
                );
            }
        }
        if !keep_peer {
            assert!(matches!(
                doc["snapshots"]["plugin@1.0.0"]["dependencies"]["engine"].as_str(),
                Some("1.0.0" | "1.1.0")
            ));
            assert_eq!(doc["snapshots"].as_object().unwrap().len(), 3);
        }
    }
}

#[test]
fn a_peer_can_use_an_external_provider_or_its_own_dependency() {
    let mut registry = Registry::default();
    for (name, version) in [("host", "1.0.0"), ("child", "1.0.0"), ("child", "2.0.0")] {
        registry.add(name, version, json!({}));
    }
    registry.add("plugin", "1.0.0", json!({
        "dependencies":{"child":"1"}, "peerDependencies":{"child":"1","plugin":"1"},
        "peerDependenciesMeta":{"child":{"optional":true},"host":{"optional":true},"unused":{"optional":false}}
    }));
    let config = json!({"autoInstallPeers":false});
    for (version, records_fallback, self_peer) in [
        ("9.15.0", false, true),
        ("9.15.1", false, false),
        ("11.28.6", false, false),
        ("11.28.7", true, false),
        ("12.14.0", false, false),
        ("12.15.0", true, false),
    ] {
        let version = version.parse().unwrap();
        for external in [false, true] {
            let mut root = json!({"dependencies":{"plugin":"1","host":"1"}});
            if external {
                root["dependencies"]["child"] = "2".into();
            }
            let manifests = workspace(&[(".", root)]);
            let input = GenerateContext::new(&manifests, &config).with_pnpm_version(&version);
            let lock = generate(&empty_workspace_lock(), &input, &mut registry).unwrap();
            let doc = lock.project().unwrap();
            let record = &doc["packages"]["plugin@1.0.0"];
            assert_eq!(record["peerDependencies"]["host"], "*");
            assert!(record["peerDependencies"].get("unused").is_none());
            assert_eq!(
                record["peerDependencies"].get("child").is_some(),
                records_fallback || external
            );
            assert_eq!(
                record["peerDependencies"].get("plugin").is_some(),
                self_peer
            );
            let pin = doc["importers"]["."]["dependencies"]["plugin"]["version"]
                .as_str()
                .unwrap();
            let body = &doc["snapshots"][format!("plugin@{pin}")];
            assert_eq!(
                pin,
                if external {
                    "1.0.0(child@2.0.0)(host@1.0.0)"
                } else {
                    "1.0.0(host@1.0.0)"
                }
            );
            assert_eq!(body["optionalDependencies"]["host"], "1.0.0");
            assert_eq!(
                body["dependencies"].get("plugin").and_then(Value::as_str),
                self_peer.then_some(pin)
            );
            if external {
                assert_eq!(body["optionalDependencies"]["child"], "2.0.0");
                assert!(body["dependencies"].get("child").is_none());
            } else {
                assert_eq!(body["dependencies"]["child"], "1.0.0");
                assert!(body["optionalDependencies"].get("child").is_none());
            }
        }
    }
}

#[test]
fn a_file_dependency_keeps_its_child_release_in_a_peer_specific_snapshot() {
    let mut registry = Registry::default();
    for name in ["host", "leaf"] {
        registry.add(name, "1.0.0", json!({}));
    }
    let config = json!({"autoInstallPeers":false});
    let mut inputs = workspace(&[
        (
            ".",
            json!({"dependencies":{"host":"1","local":"file:./local"}}),
        ),
        (
            "local",
            json!({"name":"local","version":"1.0.0","dependencies":{"leaf":"^1"},"peerDependencies":{"host":"1"}}),
        ),
    ]);
    let previous = run(
        &empty_workspace_lock(),
        &inputs,
        config.clone(),
        &mut registry,
    )
    .unwrap();
    registry.add("leaf", "1.1.0", json!({}));
    inputs.insert("later".into(), json!({"dependencies":{"leaf":"1.1.0"}}));
    let updated = run(&previous, &inputs, config.clone(), &mut registry).unwrap();
    assert_eq!(
        updated.project().unwrap()["snapshots"]["local@file:local(host@1.0.0)"]["dependencies"]["leaf"],
        "1.0.0"
    );
    let repeated = run(&updated, &inputs, config, &mut registry).unwrap();
    assert_eq!(updated.to_yaml().unwrap(), repeated.to_yaml().unwrap());
}

#[test]
fn a_hook_reads_every_release_again_once_any_project_changes() {
    struct Hook(&'static str);
    impl locksmith::GenerationHooks for Hook {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "plugin" {
                raw["dependencies"]["leaf"] = self.0.into();
            }
            Ok(raw)
        }
    }
    let mut registry = Registry::default();
    for (name, version) in [
        ("plugin", "1.0.0"),
        ("leaf", "1.0.0"),
        ("leaf", "2.0.0"),
        ("added", "1.0.0"),
    ] {
        registry.add(name, version, json!({}));
    }
    registry.add("other", "1.0.0", json!({"dependencies":{"leaf":"*"}}));
    let generate =
        |old: &Lockfile, manifests: &_, checksum: &str, leaf, registry: &mut Registry| {
            locksmith::generate_with_hooks(
                old,
                &GenerateContext::new(
                    manifests,
                    &json!({
                        "pnpmfileChecksum":checksum,
                        "overrides":{"other>leaf":"1"}
                    }),
                )
                .with_pnpm_version(&node_semver::Version::from((10, 12, 4))),
                registry,
                &mut Hook(leaf),
            )
            .unwrap()
        };
    let mut inputs = workspace(&[(".", json!({"dependencies":{"plugin":"1","other":"1"}}))]);
    let before = generate(
        &empty_workspace_lock(),
        &inputs,
        "first",
        "1",
        &mut registry,
    );
    let leaf = |lock: &Lockfile| {
        lock.project().unwrap()["snapshots"]["plugin@1.0.0"]["dependencies"]["leaf"].clone()
    };
    // A current lockfile keeps its graph even if the hook now answers differently,
    // so generation reads no manifest.
    let unchanged = generate(&before, &inputs, "first", "2", &mut Registry::default());
    assert_eq!(leaf(&unchanged), "1.0.0");
    // Adding an unrelated project makes the lockfile outdated. The hook runs
    // for the plugin again although its project and the checksum are unchanged.
    inputs.insert("new".into(), json!({"dependencies":{"added":"1"}}));
    let added = generate(&before, &inputs, "first", "2", &mut registry);
    assert_eq!(leaf(&added), "2.0.0");
    assert_eq!(
        added.project().unwrap()["snapshots"]["other@1.0.0"]["dependencies"]["leaf"],
        "1.0.0"
    );
}

#[test]
fn hook_changes_to_a_dependency_source_or_tag_update_the_lock() {
    struct Rewrite<'a>(&'a str);
    impl locksmith::GenerationHooks for Rewrite<'_> {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "plugin" {
                raw["dependencies"]["leaf"] = self.0.into();
                raw["dependenciesMeta"]["leaf"]["injected"] =
                    (self.0 == "workspace:*" || self.0.starts_with("link:")).into();
            }
            Ok(raw)
        }
    }

    let first = "https://example.test/first.tgz";
    let second = "https://example.test/second.tgz";
    let mut registry = Registry::default();
    for name in ["plugin", "leaf", "other"] {
        registry.add(name, "1.0.0", json!({}));
    }
    registry.add("leaf", "2.0.0", json!({}));
    registry.tags.insert(("leaf", "beta"), "2.0.0");
    let generate = |old: &Lockfile, manifests: &_, leaf, registry: &mut Registry| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(manifests, &json!({"pnpmfileChecksum":"stable"})),
            registry,
            &mut Rewrite(leaf),
        )
        .unwrap()
    };
    let mut inputs = workspace(&[
        (".", json!({"dependencies":{"plugin":"1"}})),
        ("packages/a", json!({"name":"a","version":"1.0.0"})),
        ("packages/b", json!({"name":"leaf","version":"1.0.0"})),
    ]);
    let mut old = generate(&empty_workspace_lock(), &inputs, "mirror:1", &mut registry);
    let leaf = |lock: &Lockfile| {
        lock.project().unwrap()["snapshots"]["plugin@1.0.0"]["dependencies"]["leaf"].clone()
    };
    assert_eq!(leaf(&old), "mirror:1.0.0");
    for (member, selector, expected) in [
        ("same-mirror", "mirror:1", "mirror:1.0.0"),
        ("default-source", "1", "1.0.0"),
        ("tag", "beta", "2.0.0"),
        ("workspace-a", "workspace:./packages/a", "link:packages/a"),
        ("workspace-b", "workspace:./packages/b", "link:packages/b"),
        ("injected", "workspace:*", "file:packages/b"),
        ("explicit-link", "link:./packages/b", "link:packages/b"),
    ] {
        inputs.insert(member.into(), json!({"dependencies":{"other":"1"}}));
        old = generate(&old, &inputs, selector, &mut registry);
        assert_eq!(leaf(&old), expected, "{selector}");
    }
    for (member, source, needs_fetch) in [
        ("one", first, true),
        ("two", first, false),
        ("three", second, true),
    ] {
        inputs.insert(member.into(), json!({"dependencies":{"other":"1"}}));
        registry.tarballs.clear();
        if needs_fetch {
            registry.tarballs.insert(source.into());
        }
        old = generate(&old, &inputs, source, &mut registry);
        let doc = old.project().unwrap();
        assert_eq!(
            doc["snapshots"]["plugin@1.0.0"]["dependencies"]["leaf"],
            source
        );
        assert_eq!(
            doc["packages"][format!("leaf@{source}")]["resolution"]["tarball"],
            source
        );
    }
}

#[test]
fn overrides_remove_peers_before_the_old_graph_is_reused() {
    struct Rewrite(&'static str);
    impl locksmith::GenerationHooks for Rewrite {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "plugin" {
                raw["peerDependencies"] = json!({(self.0):"*"});
            }
            Ok(raw)
        }
    }

    let archive = "https://example.test/leaf.tgz";
    let mut registry = Registry::default();
    registry.add("plugin", "1.0.0", json!({"dependencies":{"leaf":archive}}));
    registry.tarballs.insert(archive.into());
    let config = json!({
        "pnpmfileChecksum":"stable",
        "autoInstallPeers":false,
        "overrides":{"plugin>host":"-","plugin>promoted":"link:packages/provider"}
    });
    let mut inputs = workspace(&[
        (".", json!({"dependencies":{"plugin":"1"}})),
        (
            "packages/provider",
            json!({"name":"provider","version":"1.0.0"}),
        ),
    ]);
    let generate = |old: &Lockfile, manifests: &_, peer, registry: &mut Registry| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(manifests, &config),
            registry,
            &mut Rewrite(peer),
        )
        .unwrap()
    };
    let old = generate(&empty_workspace_lock(), &inputs, "ghost", &mut registry);
    assert_eq!(
        old.project().unwrap()["packages"]["plugin@1.0.0"]["peerDependencies"],
        json!({"ghost":"*"})
    );
    inputs.insert("one".into(), json!({}));
    let next = generate(&old, &inputs, "host", &mut registry);
    let record = &next.project().unwrap()["packages"]["plugin@1.0.0"];
    assert!(record.get("peerDependencies").is_none());
    // The effective peer map is still empty. The existing archive pin is
    // sufficient when a second unrelated importer changes the lockfile.
    registry.tarballs.clear();
    inputs.insert("two".into(), json!({}));
    let unchanged = generate(&next, &inputs, "host", &mut registry);
    assert_eq!(
        unchanged.project().unwrap()["packages"]["plugin@1.0.0"],
        *record
    );

    registry.tarballs.insert(archive.into());
    inputs.insert("three".into(), json!({}));
    let promoted = generate(&unchanged, &inputs, "promoted", &mut registry);
    let project = promoted.project().unwrap();
    assert!(
        project["packages"]["plugin@1.0.0"]
            .get("peerDependencies")
            .is_none()
    );
    assert_eq!(
        project["snapshots"]["plugin@1.0.0"]["dependencies"]["promoted"],
        "link:packages/provider"
    );
    // An ordinary link introduced by a peer override is sufficient evidence on reuse.
    registry.tarballs.clear();
    inputs.insert("four".into(), json!({}));
    let again = generate(&promoted, &inputs, "promoted", &mut registry);
    assert_eq!(again.project().unwrap()["snapshots"], project["snapshots"]);
}

#[test]
fn history_keeps_the_optional_group_and_ignores_bundled_dependencies() {
    struct Identity;
    impl locksmith::GenerationHooks for Identity {
        fn read_package(&mut self, _: &str, raw: Value) -> Result<Value> {
            Ok(raw)
        }
    }

    let archive = "https://example.test/leaf.tgz";
    let mut registry = Registry::default();
    registry.tarballs.insert(archive.into());
    registry.add("optional", "1.0.0", json!({}));
    registry.add(
        "plugin",
        "1.0.0",
        json!({
            "dependencies":{"leaf":archive,"optional":"2","ignored":"1","removed":"1","bundled":"1"},
            "optionalDependencies":{"optional":"1","ignored":"1","removed":"2"},
            "bundledDependencies":["bundled"],
            "bundleDependencies":["leaf"]
        }),
    );
    registry.add(
        "all-bundled",
        "1.0.0",
        json!({"dependencies":{"bundled":"1"},"bundledDependencies":true,"bundleDependencies":[]}),
    );
    let config = json!({
        "pnpmfileChecksum":"stable",
        "ignoredOptionalDependencies":["ignored"],
        "overrides":{"plugin>removed@2":"-"}
    });
    let mut inputs = workspace(&[(
        ".",
        json!({"dependencies":{"plugin":"1","all-bundled":"1"}}),
    )]);
    let generate = |old: &Lockfile, manifests: &_, registry: &mut Registry| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(manifests, &config),
            registry,
            &mut Identity,
        )
        .unwrap()
    };
    let old = generate(&empty_workspace_lock(), &inputs, &mut registry);
    let graph = &old.project().unwrap()["snapshots"];
    assert_eq!(
        graph["plugin@1.0.0"]["dependencies"],
        json!({"leaf":archive})
    );
    assert_eq!(
        graph["plugin@1.0.0"]["optionalDependencies"],
        json!({"optional":"1.0.0"})
    );
    assert!(graph["all-bundled@1.0.0"].get("dependencies").is_none());
    registry.tarballs.clear();
    inputs.insert("added".into(), json!({}));
    let next = generate(&old, &inputs, &mut registry);
    assert_eq!(next.project().unwrap()["snapshots"], *graph);
}

#[test]
fn history_updates_a_packages_own_peer_fallback_and_keeps_external_bindings() {
    struct Rewrite(&'static str);
    impl locksmith::GenerationHooks for Rewrite {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "plugin" {
                raw["dependencies"]["host"] = self.0.into();
            }
            Ok(raw)
        }
    }

    let mut registry = Registry::default();
    registry.add(
        "plugin",
        "1.0.0",
        json!({
            "dependencies":{"host":"^1"},
            "peerDependencies":{"host":"*"}
        }),
    );
    for version in ["1.0.0", "2.0.0"] {
        registry.add("host", version, json!({}));
    }
    let config = json!({"pnpmfileChecksum":"stable","autoInstallPeers":false});
    let version = node_semver::Version::from((12, 15, 0));
    let mut inputs = workspace(&[
        (".", json!({"dependencies":{"plugin":"1"}})),
        (
            "with-host",
            json!({"dependencies":{"plugin":"1","host":"1"}}),
        ),
    ]);
    let generate = |old: &Lockfile, manifests: &_, range, registry: &mut Registry| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(manifests, &config).with_pnpm_version(&version),
            registry,
            &mut Rewrite(range),
        )
        .unwrap()
    };
    let old = generate(&empty_workspace_lock(), &inputs, "^1", &mut registry);
    assert_eq!(
        old.project().unwrap()["snapshots"]["plugin@1.0.0"]["dependencies"]["host"],
        "1.0.0"
    );
    inputs.insert("unrelated".into(), json!({}));
    let next = generate(&old, &inputs, "^2", &mut registry);
    let doc = next.project().unwrap();
    assert_eq!(
        doc["snapshots"]["plugin@1.0.0"]["dependencies"]["host"],
        "2.0.0"
    );
    assert_eq!(
        doc["snapshots"]["plugin@1.0.0(host@1.0.0)"]["dependencies"]["host"],
        "1.0.0"
    );
}

#[test]
fn a_directory_source_identifies_the_same_path_in_its_locator_and_resolution() {
    let manifest = json!({"name":"child","version":"1.0.0"});
    assert!(
        SourceManifest::new(
            "child@file:packages/child",
            manifest.clone(),
            json!({"type":"directory","directory":"packages/child"}),
        )
        .is_ok()
    );
    let error = SourceManifest::new(
        "child@file:packages/child",
        manifest,
        json!({"type":"directory","directory":"packages/other"}),
    )
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("directory must match the file: package locator")
    );
}

#[test]
fn history_updates_a_peer_range_even_when_the_package_provides_that_peer() {
    struct Rewrite(&'static str);
    impl locksmith::GenerationHooks for Rewrite {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "plugin" {
                raw["peerDependencies"]["host"] = self.0.into();
            }
            Ok(raw)
        }
    }
    let mut registry = Registry::default();
    registry.add("plugin", "1.0.0", json!({"dependencies":{"host":"1"}}));
    registry.add("host", "1.0.0", json!({}));
    let config = json!({"pnpmfileChecksum":"stable","autoInstallPeers":false});
    let version = node_semver::Version::from((12, 15, 0));
    let mut inputs = workspace(&[(".", json!({"dependencies":{"plugin":"1"}}))]);
    let generate = |old: &Lockfile, manifests: &_, range, registry: &mut Registry| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(manifests, &config).with_pnpm_version(&version),
            registry,
            &mut Rewrite(range),
        )
        .unwrap()
    };
    let old = generate(&empty_workspace_lock(), &inputs, "^1", &mut registry);
    assert_eq!(
        old.project().unwrap()["packages"]["plugin@1.0.0"]["peerDependencies"]["host"],
        "^1"
    );
    inputs.insert("added".into(), json!({}));
    let next = generate(&old, &inputs, "*", &mut registry);
    assert_eq!(
        next.project().unwrap()["packages"]["plugin@1.0.0"]["peerDependencies"]["host"],
        "*"
    );
}

#[test]
fn a_hook_rewrite_that_the_lock_still_satisfies_keeps_its_releases() {
    // The rewrite widens a range the locked child already satisfies. The
    // release also declares an optional peer only in peerDependenciesMeta,
    // which pnpm records as a peer.
    struct Widen;
    impl locksmith::GenerationHooks for Widen {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "plugin" {
                raw["dependencies"]["leaf"] = "^1.0.0".into();
            }
            Ok(raw)
        }
    }
    let mut registry = Registry::default();
    registry.add(
        "plugin",
        "1.0.0",
        json!({"dependencies":{"leaf":"1.0.0"},"peerDependenciesMeta":{"extra":{"optional":true}}}),
    );
    registry.add("leaf", "1.0.0", json!({}));
    registry.add("added", "1.0.0", json!({}));
    let config = json!({"pnpmfileChecksum":"hook"});
    let version = node_semver::Version::from((10, 12, 4));
    let mut inputs = workspace(&[(".", json!({"dependencies":{"plugin":"1"}}))]);
    let generate = |old: &Lockfile, inputs: &BTreeMap<String, Value>, registry: &mut Registry| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(inputs, &config).with_pnpm_version(&version),
            registry,
            &mut Widen,
        )
        .unwrap()
    };
    let before = generate(&empty_workspace_lock(), &inputs, &mut registry);
    assert!(
        before.project().unwrap()["packages"]["plugin@1.0.0"]["peerDependencies"]
            .get("extra")
            .is_some()
    );
    // A newer release would be selected if the plugin were resolved again.
    registry.add("leaf", "1.1.0", json!({}));
    inputs.insert("new".into(), json!({"dependencies":{"added":"1"}}));
    let after = generate(&before, &inputs, &mut registry);
    assert_eq!(
        after.project().unwrap()["snapshots"]["plugin@1.0.0"]["dependencies"]["leaf"],
        "1.0.0"
    );
}

#[test]
fn a_changed_hook_rewrite_resolves_its_release_again() {
    struct Rewrite(&'static str);
    impl locksmith::GenerationHooks for Rewrite {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] != "plugin" {
                return Ok(raw);
            }
            match self.0 {
                "added" => raw["dependencies"]["extra"] = "1".into(),
                "optional" => {
                    raw["dependencies"].as_object_mut().unwrap().remove("leaf");
                    raw["optionalDependencies"] = json!({"leaf":"1"});
                }
                "alias" => raw["dependencies"]["tool"] = "npm:other@^1".into(),
                "peer" => raw["peerDependencies"]["host"] = "^2".into(),
                _ => {}
            }
            Ok(raw)
        }
    }
    let mut registry = Registry::default();
    registry.add(
        "plugin",
        "1.0.0",
        json!({
            "dependencies":{"leaf":"1","tool":"npm:first@^1"},
            "peerDependencies":{"host":"^1"}
        }),
    );
    for name in ["leaf", "extra", "first", "other", "added"] {
        registry.add(name, "1.0.0", json!({}));
    }
    registry.add("host", "1.0.0", json!({}));
    registry.add("host", "2.0.0", json!({}));
    let config = json!({
        "pnpmfileChecksum":"hook",
        "autoInstallPeers":false,
        "overrides":{"unrelated>host":"^1"}
    });
    let version = node_semver::Version::from((10, 12, 4));
    let mut generate = |old: &Lockfile, inputs: &BTreeMap<String, Value>, hook| {
        locksmith::generate_with_hooks(
            old,
            &GenerateContext::new(inputs, &config).with_pnpm_version(&version),
            &mut registry,
            &mut Rewrite(hook),
        )
        .unwrap()
    };
    let inputs = workspace(&[(".", json!({"dependencies":{"plugin":"1","host":"1"}}))]);
    let mut edited = inputs.clone();
    edited.insert("new".into(), json!({"dependencies":{"added":"1"}}));
    let plugin = |lock: &Lockfile| {
        let doc = lock.project().unwrap();
        let key = doc["snapshots"]
            .as_object()
            .unwrap()
            .keys()
            .find(|key| key.starts_with("plugin@"))
            .unwrap()
            .clone();
        (
            doc["packages"]["plugin@1.0.0"].clone(),
            doc["snapshots"][&key].clone(),
        )
    };
    // The hook stops adding a dependency.
    let before = generate(&empty_workspace_lock(), &inputs, "added");
    let (_, body) = plugin(&generate(&before, &edited, "none"));
    assert!(body["dependencies"].get("extra").is_none());
    // The hook moves a dependency to the optional group, retargets an alias
    // and changes a peer range, each with the same pnpmfile checksum.
    let before = generate(&empty_workspace_lock(), &inputs, "none");
    let (_, body) = plugin(&generate(&before, &edited, "optional"));
    assert_eq!(body["optionalDependencies"]["leaf"], "1.0.0");
    let (_, body) = plugin(&generate(&before, &edited, "alias"));
    assert_eq!(body["dependencies"]["tool"], "other@1.0.0");
    let (record, _) = plugin(&generate(&before, &edited, "peer"));
    assert_eq!(record["peerDependencies"]["host"], "^2");
}

#[test]
fn an_automatic_required_peer_follows_the_pnpm_release_rule() {
    let mut registry = Registry::default();
    registry.add("host", "1.0.0", json!({}));
    registry.add("host", "2.0.0", json!({}));
    registry.add("plugin", "1.0.0", json!({"peerDependencies":{"host":"^2"}}));
    let mut manifests = workspace(&[
        (".", json!({})),
        ("existing", json!({"dependencies":{"host":"1"}})),
    ]);
    let cfg = json!({"autoInstallPeers":true,"resolvePeersFromWorkspaceRoot":false});
    let before = run(
        &empty_workspace_lock(),
        &manifests,
        cfg.clone(),
        &mut registry,
    )
    .unwrap();
    manifests.insert("new".into(), json!({"dependencies":{"plugin":"1"}}));
    // pnpm 12 resolves a release in the peer's range.
    let after = run(&before, &manifests, cfg.clone(), &mut registry).unwrap();
    let doc = after.project().unwrap();
    assert_eq!(
        doc["importers"]["new"]["dependencies"]["plugin"]["version"],
        "1.0.0(host@2.0.0)"
    );
    assert_eq!(
        before.dependency_closure("existing").unwrap(),
        after.dependency_closure("existing").unwrap()
    );
    // pnpm 9 and 10 reuse the newest locked release whatever the range.
    let legacy = generate(
        &before,
        &GenerateContext::new(&manifests, &cfg)
            .with_pnpm_version(&node_semver::Version::from((10, 12, 4))),
        &mut registry,
    )
    .unwrap();
    let doc = legacy.project().unwrap();
    assert_eq!(
        doc["importers"]["new"]["dependencies"]["plugin"]["version"],
        "1.0.0(host@1.0.0)"
    );
    assert!(doc["packages"].get("host@2.0.0").is_none());
}

#[test]
fn an_automatic_optional_peer_uses_the_highest_compatible_locked_release() {
    let mut registry = Registry::default();
    for v in ["1.0.0", "1.1.0", "1.2.0", "2.0.0"] {
        registry.add("host", v, json!({}));
    }
    registry.add(
        "plugin", "1.0.0",
        json!({"peerDependencies":{"host":"^1","absent":"*"},"peerDependenciesMeta":{"host":{"optional":true},"absent":{"optional":true}}}),
    );
    let mut manifests = workspace(&[
        (".", json!({})),
        ("one", json!({"dependencies":{"host":"1.0.0"}})),
        ("two", json!({"dependencies":{"host":"1.1.0"}})),
        ("three", json!({"dependencies":{"host":"2.0.0"}})),
    ]);
    let cfg = json!({"autoInstallPeers":true,"resolvePeersFromWorkspaceRoot":false});
    let before = run(
        &empty_workspace_lock(),
        &manifests,
        cfg.clone(),
        &mut registry,
    )
    .unwrap();
    manifests.insert("new".into(), json!({"dependencies":{"plugin":"1"}}));
    let after = run(&before, &manifests, cfg.clone(), &mut registry).unwrap();
    let doc = after.project().unwrap();
    assert_eq!(
        doc["importers"]["new"]["dependencies"]["plugin"]["version"],
        "1.0.0(host@1.1.0)"
    );
    assert_eq!(
        doc["snapshots"]["plugin@1.0.0(host@1.1.0)"]["optionalDependencies"]["host"],
        "1.1.0"
    );
    assert!(doc["packages"].get("host@1.2.0").is_none());
    let repeated = run(&after, &manifests, cfg, &mut registry).unwrap();
    assert_eq!(repeated.to_yaml().unwrap(), after.to_yaml().unwrap());
}

#[test]
fn consumers_with_disjoint_peer_ranges_still_get_an_automatically_installed_provider() {
    let mut registry = Registry::default();
    registry.add(
        "old-plugin",
        "1.0.0",
        json!({"peerDependencies":{"host":"^1"}}),
    );
    registry.add(
        "new-plugin",
        "1.0.0",
        json!({"peerDependencies":{"host":"^2"}}),
    );
    for version in ["1.0.0", "2.0.0"] {
        registry.add("host", version, json!({}));
    }
    let manifests = workspace(&[(
        ".",
        json!({"dependencies":{"old-plugin":"1","new-plugin":"1"}}),
    )]);
    let lock = run(
        &empty_workspace_lock(),
        &manifests,
        json!({}),
        &mut registry,
    )
    .unwrap();
    let doc = lock.project().unwrap();
    let provider = |name: &str| {
        let pin = doc["importers"]["."]["dependencies"][name]["version"]
            .as_str()
            .unwrap();
        doc["snapshots"][format!("{name}@{pin}")]["dependencies"]["host"].clone()
    };
    // One release cannot satisfy both ranges, but leaving the peer out would
    // break both consumers. They share one provider instead.
    assert!(provider("old-plugin").is_string());
    assert_eq!(provider("old-plugin"), provider("new-plugin"));
}

#[test]
fn optional_peer_metadata_does_not_declare_a_peer_the_package_installs_itself() {
    let mut registry = Registry::default();
    registry.add("child", "1.0.0", json!({}));
    registry.add("plugin", "1.0.0", json!({
        "dependencies":{"child":"1"},
        "optionalDependencies":{"extra":"1"},
        "peerDependenciesMeta":{"child":{"optional":true},"extra":{"optional":true},"host":{"optional":true}}
    }));
    registry.add("extra", "1.0.0", json!({}));
    // pnpm treats optional metadata without a range as a "*" peer, but skips
    // names the package installs itself. Releases that record fallback peers
    // keep an ordinary dependency and still skip an optional one.
    let manifests = workspace(&[(".", json!({"dependencies":{"plugin":"1"}}))]);
    let config = json!({"autoInstallPeers":false});
    for (version, keeps_dependency) in [("10.12.4", false), ("11.28.6", false), ("11.28.7", true)] {
        let pnpm_version = version.parse().unwrap();
        let input = GenerateContext::new(&manifests, &config).with_pnpm_version(&pnpm_version);
        let lock = generate(&empty_workspace_lock(), &input, &mut registry).unwrap();
        let peers = &lock.project().unwrap()["packages"]["plugin@1.0.0"]["peerDependencies"];
        assert_eq!(peers["host"], "*", "{version}");
        assert!(peers.get("extra").is_none(), "{version}");
        assert_eq!(peers.get("child").is_some(), keeps_dependency, "{version}");
    }
}

#[test]
fn the_builtin_database_extends_published_manifests_before_the_project_hook() {
    struct ReadExtensions;
    impl locksmith::GenerationHooks for ReadExtensions {
        fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
            if raw["name"] == "babel-plugin-transform-typescript-metadata"
                && raw["peerDependencies"]["@babel/core"] == "^7"
            {
                raw["dependencies"]["seen-by-hook"] = "1".into();
                raw["peerDependencies"]
                    .as_object_mut()
                    .unwrap()
                    .remove("@babel/traverse");
                raw["peerDependenciesMeta"]
                    .as_object_mut()
                    .unwrap()
                    .remove("@babel/traverse");
            }
            Ok(raw)
        }
    }
    let mut registry = Registry::default();
    for (name, version) in [
        ("anchor", "1.0.0"),
        ("seen-by-hook", "1.0.0"),
        ("babel-plugin-transform-typescript-metadata", "0.3.2"),
    ] {
        registry.add(name, version, json!({}));
    }
    let mut manifests = workspace(&[(".", json!({"dependencies":{"anchor":"1"}}))]);
    let old = run(
        &empty_workspace_lock(),
        &manifests,
        json!({}),
        &mut registry,
    )
    .unwrap();
    manifests.get_mut(".").unwrap()["dependencies"]["babel-plugin-transform-typescript-metadata"] =
        "0.3.2".into();
    for ignored in [false, true] {
        let version = node_semver::Version::from((10, 12, 4));
        let lock = locksmith::generate_with_hooks(
            &old,
            &GenerateContext::new(
                &manifests,
                &json!({"autoInstallPeers":false,"ignoreCompatibilityDb":ignored}),
            )
            .with_pnpm_version(&version),
            &mut registry,
            &mut ReadExtensions,
        )
        .unwrap();
        let doc = lock.project().unwrap();
        let key = "babel-plugin-transform-typescript-metadata@0.3.2";
        assert_eq!(
            doc["packages"][key]["peerDependencies"]["@babel/core"].as_str(),
            (!ignored).then_some("^7")
        );
        assert!(
            doc["packages"][key]["peerDependencies"]
                .get("@babel/traverse")
                .is_none()
        );
        assert_eq!(
            doc["snapshots"][key]["dependencies"]["seen-by-hook"].as_str(),
            (!ignored).then_some("1.0.0")
        );
    }
}

#[test]
fn toggling_the_builtin_database_applies_when_the_lockfile_next_changes() {
    let mut registry = Registry::default();
    for (name, version) in [
        ("anchor", "1.0.0"),
        ("babel-plugin-transform-typescript-metadata", "0.3.2"),
    ] {
        registry.add(name, version, json!({}));
    }
    let key = "babel-plugin-transform-typescript-metadata@0.3.2";
    let declares_peer = |lock: &Lockfile| {
        lock.project().unwrap()["packages"][key]["peerDependencies"]
            .get("@babel/core")
            .is_some()
    };
    let mut manifests = workspace(&[(
        ".",
        json!({"dependencies":{"babel-plugin-transform-typescript-metadata":"0.3.2"}}),
    )]);
    for ignored in [false, true] {
        let config =
            |ignore: bool| json!({"autoInstallPeers":false,"ignoreCompatibilityDb":ignore});
        let old = run(
            &empty_workspace_lock(),
            &manifests,
            config(ignored),
            &mut registry,
        )
        .unwrap();
        assert_eq!(declares_peer(&old), !ignored);
        // The lockfile does not record the setting. pnpm leaves a current lockfile alone.
        let current = run(&old, &manifests, config(!ignored), &mut Registry::default()).unwrap();
        assert_eq!(declares_peer(&current), !ignored);
        manifests.get_mut(".").unwrap()["dependencies"]["anchor"] = "1".into();
        let edited = run(&old, &manifests, config(!ignored), &mut registry).unwrap();
        assert_eq!(declares_peer(&edited), ignored);
        manifests.get_mut(".").unwrap()["dependencies"]
            .as_object_mut()
            .unwrap()
            .remove("anchor");
    }
}

struct UpdateLeaf;

impl locksmith::GenerationHooks for UpdateLeaf {
    fn read_package(&mut self, _: &str, mut raw: Value) -> Result<Value> {
        if raw["name"] == "changing" {
            raw["dependencies"]["leaf"] = "2".into();
        }
        Ok(raw)
    }
}

#[test]
fn an_edited_subtree_keeps_each_siblings_peer_bound_to_its_actual_consumer() {
    use locksmith::generate_with_hooks;
    let mut registry = Registry::default();
    for name in ["host", "leaf"] {
        for version in ["1.0.0", "2.0.0"] {
            registry.add(name, version, json!({}));
        }
    }
    registry.add(
        "tree",
        "1.0.0",
        json!({"dependencies":{"stable":"1","changing":"1"}}),
    );
    registry.add("stable", "1.0.0", json!({"peerDependencies":{"host":"*"}}));
    registry.add("changing", "1.0.0", json!({"dependencies":{"leaf":"1"}}));
    let app = |host: &str| json!({"dependencies":{"tree":"1","host":host}});
    let config = json!({"autoInstallPeers":false, "resolvePeersFromWorkspaceRoot":false});
    let before = workspace(&[(".", json!({})), ("a", app("1")), ("b", app("1"))]);
    let after = workspace(&[(".", json!({})), ("a", app("2")), ("b", app("1"))]);
    let old = run(
        &empty_workspace_lock(),
        &before,
        config.clone(),
        &mut registry,
    )
    .unwrap();
    let input = GenerateContext::new(&after, &config);
    let next = generate_with_hooks(&old, &input, &mut registry, &mut UpdateLeaf).unwrap();
    let doc = next.project().unwrap();
    for (id, host) in [("a", "2.0.0"), ("b", "1.0.0")] {
        let root = doc["importers"][id]["dependencies"]["tree"]["version"]
            .as_str()
            .unwrap();
        let edges = &doc["snapshots"][format!("tree@{root}")]["dependencies"];
        let pin = edges["stable"].as_str().unwrap();
        assert_eq!(
            doc["snapshots"][format!("stable@{pin}")]["dependencies"]["host"],
            host
        );
        let pin = edges["changing"].as_str().unwrap();
        assert_eq!(
            doc["snapshots"][format!("changing@{pin}")]["dependencies"]["leaf"],
            "2.0.0"
        );
    }
    let repeated = generate_with_hooks(&next, &input, &mut registry, &mut UpdateLeaf).unwrap();
    assert_eq!(next.to_yaml().unwrap(), repeated.to_yaml().unwrap());
}

#[test]
fn retained_children_contribute_only_the_optional_peers_they_bound() {
    let mut registry = Registry::default();
    for version in ["1.0.0", "2.0.0"] {
        registry.add("leaf", version, json!({}));
        registry.add("host", version, json!({}));
    }
    registry.add(
        "stable",
        "1.0.0",
        json!({
            "peerDependencies": {"host": "*"}, "peerDependenciesMeta": {"host": {"optional": true}}
        }),
    );
    registry.add("changing", "1.0.0", json!({"dependencies": {"leaf": "1"}}));
    registry.add(
        "tree",
        "1.0.0",
        json!({"dependencies": {"stable": "1", "changing": "1"}}),
    );
    let settings = json!({"autoInstallPeers":false,"resolvePeersFromWorkspaceRoot":false});
    let mut manifests = workspace(&[
        (".", json!({"dependencies":{"tree":"1"}})),
        ("tools", json!({"dependencies":{"host":"1"}})),
    ]);
    let old = run(
        &empty_workspace_lock(),
        &manifests,
        settings.clone(),
        &mut registry,
    )
    .unwrap();
    // A lock can contain an earlier unbound optional peer even when that
    // provider is now present in the same importer's dependency declarations.
    let mut old = old.project().unwrap().clone();
    old["importers"]["."]["dependencies"]["host"] =
        old["importers"]["tools"]["dependencies"]["host"].clone();
    let old = Lockfile::parse(&old.to_string()).unwrap();
    for (selector, expected) in [("1", None), ("2", Some("2.0.0"))] {
        manifests.get_mut(".").unwrap()["dependencies"]["host"] = selector.into();
        let input = GenerateContext::new(&manifests, &settings);
        let next =
            locksmith::generate_with_hooks(&old, &input, &mut registry, &mut UpdateLeaf).unwrap();
        let doc = next.project().unwrap();
        let tree = doc["importers"]["."]["dependencies"]["tree"]["version"]
            .as_str()
            .unwrap();
        let tree_edges = &doc["snapshots"][format!("tree@{tree}")]["dependencies"];
        let stable = tree_edges["stable"].as_str().unwrap();
        assert_eq!(
            doc["snapshots"][format!("stable@{stable}")]["optionalDependencies"]["host"].as_str(),
            expected
        );
        assert_eq!(tree.contains("(host@"), expected.is_some());
        let changing = tree_edges["changing"].as_str().unwrap();
        assert_eq!(
            doc["snapshots"][format!("changing@{changing}")]["dependencies"]["leaf"],
            "2.0.0"
        );
    }
}

#[test]
fn a_link_override_preserves_the_declared_dev_group_with_pnpm_12_17() {
    let manifests = workspace(&[
        (".", json!({})),
        (
            "app",
            json!({"devDependencies":{"sdk":"^1"},"peerDependencies":{"sdk":"^1"}}),
        ),
        ("sdk", json!({"name":"sdk","version":"1.0.0"})),
    ]);
    let previous = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {'.': {}, app: {dependencies: {sdk: {specifier: 'link:../sdk', version: 'link:../sdk'}}}, sdk: {}}\n").unwrap();
    for (version, group) in [
        ((9, 14, 4), "devDependencies"),
        ((10, 12, 4), "dependencies"),
        ((12, 16, 1), "dependencies"),
        ((12, 17, 0), "devDependencies"),
    ] {
        let (major, minor, patch) = version;
        let lock = generate(
            &previous,
            &GenerateContext::new(&manifests, &json!({"overrides":{"sdk":"link:sdk"}}))
                .with_pnpm_version(&node_semver::Version::from((major, minor, patch))),
            &mut Registry::default(),
        )
        .unwrap();
        assert_eq!(
            lock.project().unwrap()["importers"]["app"],
            json!({(group):{"sdk":{"specifier":"link:../sdk","version":"link:../sdk"}}})
        );
    }
}

#[test]
fn linked_catalog_entries_follow_the_workspace_pnpm_version() {
    let manifests = workspace(&[
        (".", json!({})),
        ("apps/app", json!({"dependencies":{"sdk":"catalog:tools"}})),
        (
            "packages/sdk",
            json!({"name":"sdk","version":"1.0.0-beta.2"}),
        ),
    ]);
    let settings = json!({
        "catalogs":{"tools":{"sdk":"^1.0.0-beta.2"}},
        "linkWorkspacePackages":true
    });
    let previous = empty_workspace_lock();
    for (pnpm, expected) in [
        ("9.15.9", Some("1.0.0-beta.2")),
        ("10.34.9", Some("1.0.0-beta.2")),
        ("11.28.12", Some("1.0.0-beta.2")),
        ("12.5.1", None),
        ("12.6.0", Some("^1.0.0-beta.2")),
        ("12.21.0", Some("^1.0.0-beta.2")),
    ] {
        let version = pnpm.parse().unwrap();
        let lock = generate(
            &previous,
            &GenerateContext::new(&manifests, &settings).with_pnpm_version(&version),
            &mut Registry::default(),
        )
        .unwrap();
        let doc = lock.project().unwrap();
        assert_eq!(
            doc["catalogs"]["tools"].get("sdk").cloned(),
            expected.map(|v| json!({"specifier":"^1.0.0-beta.2", "version":v})),
            "{pnpm}"
        );
        assert_eq!(
            doc["importers"]["apps/app"]["dependencies"]["sdk"]["version"],
            "link:../../packages/sdk"
        );
        lock.validate().unwrap();
    }
}

#[test]
fn circular_dependencies_with_an_inherited_peer_reach_a_finite_graph() {
    let mut registry = Registry::default();
    registry.add("a", "1.0.0", json!({"dependencies":{"b":"1.0.0"}}));
    registry.add(
        "b",
        "1.0.0",
        json!({"dependencies":{"a":"1.0.0"},"peerDependencies":{"p":"^2"}}),
    );
    registry.add("p", "2.0.0", json!({}));
    let inputs = workspace(&[(".", json!({"dependencies":{"a":"1.0.0","p":"2.0.0"}}))]);
    let lock = run(
        &empty_workspace_lock(),
        &inputs,
        json!({"strictPeerDependencies":true}),
        &mut registry,
    )
    .unwrap();
    let graph = lock.project().unwrap();
    assert_eq!(
        graph["snapshots"]["a@1.0.0(p@2.0.0)"]["dependencies"]["b"],
        "1.0.0(p@2.0.0)"
    );
    assert_eq!(
        graph["snapshots"]["b@1.0.0(p@2.0.0)"]["dependencies"]["a"],
        "1.0.0(p@2.0.0)"
    );
    assert_eq!(lock.dependency_closure(".").unwrap().snapshots.len(), 3);
}
