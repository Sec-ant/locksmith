use locksmith::{Lockfile, LockfileError};
use proptest::{
    collection::btree_map, prelude::ProptestConfig, prop_assert, prop_assert_eq, proptest,
};
use serde_json::{Value, json};

fn lock(dependencies: &[(u8, u8)]) -> Lockfile {
    let mut importer = serde_json::Map::new();
    let mut packages = serde_json::Map::new();
    let mut snapshots = serde_json::Map::new();
    for &(id, version) in dependencies {
        let name = format!("pkg-{id}");
        let version = format!("{version}.0.0");
        importer.insert(name.clone(), json!({"specifier":version,"version":version}));
        packages.insert(
            format!("{name}@{version}"),
            json!({"resolution":{"integrity":"sha512-test"}}),
        );
        snapshots.insert(format!("{name}@{version}"), json!({}));
    }
    let document = json!({
        "lockfileVersion": "9.0",
        "importers": {".": {"dependencies": importer}},
        "packages": packages,
        "snapshots": snapshots,
    });
    Lockfile::parse(&document.to_string()).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn independent_additions_are_symmetric_and_lossless(
        left in btree_map(0u8..32, 1u8..5, 0..24),
        right in btree_map(32u8..64, 1u8..5, 0..24),
    ) {
        let left: Vec<_> = left.into_iter().collect();
        let right: Vec<_> = right.into_iter().collect();
        let base = lock(&[]);
        let ours = lock(&left);
        let theirs = lock(&right);
        let merged = locksmith::merge(&base, &ours, &theirs, None).unwrap();
        let reversed = locksmith::merge(&base, &theirs, &ours, None).unwrap();
        prop_assert_eq!(merged.project(), reversed.project());
        let expected = lock(&left.into_iter().chain(right).collect::<Vec<_>>());
        prop_assert_eq!(merged.project(), expected.project());
        let parsed = Lockfile::parse(&merged.to_yaml().unwrap()).unwrap();
        prop_assert_eq!(parsed.project(), merged.project());
    }

    #[test]
    fn malformed_input_never_panics(text in ".{0,4096}") {
        if let Ok(lockfile) = Lockfile::parse(&text) {
            let _ = lockfile.validate();
            prop_assert!(lockfile.to_yaml().is_ok());
        }
    }

    #[test]
    fn opaque_metadata_survives_without_interpretation(
        key in "[a-z]{1,20}",
        value in ".{0,128}",
    ) {
        let document = json!({
            "lockfileVersion": "9.0",
            "importers": {".": {}},
            "extension": {key: value},
        });
        let base = Lockfile::parse(&serde_saphyr::to_string(&document).unwrap()).unwrap();
        let donor = Lockfile::parse(
            "lockfileVersion: '9.0'\nimporters: {'.': {publishDirectory: dist}}\n"
        ).unwrap();
        let result = locksmith::merge(&lock(&[]), &base, &donor, None).unwrap();
        let parsed = Lockfile::parse(&result.to_yaml().unwrap()).unwrap();
        prop_assert_eq!(&parsed.project().unwrap()["extension"], &document["extension"]);
    }
}

#[test]
fn exhaustive_single_dependency_three_way_contract() {
    // Domain states are absent, or a concrete major version. Enumerate every
    // add/delete/update combination rather than sampling implementation branches.
    for base in 0..4 {
        for ours in 0..4 {
            for theirs in 0..4 {
                let input = |state| lock(&if state == 0 { vec![] } else { vec![(1, state)] });
                let result = locksmith::merge(&input(base), &input(ours), &input(theirs), None);
                let expected = if ours == theirs {
                    Some(ours)
                } else if ours == base {
                    Some(theirs)
                } else if theirs == base {
                    Some(ours)
                } else {
                    None
                };
                match expected {
                    Some(expected) => {
                        assert_eq!(result.unwrap().project(), input(expected).project())
                    }
                    None => assert!(
                        matches!(result, Err(LockfileError::Conflicts(_))),
                        "base={base} ours={ours} theirs={theirs}"
                    ),
                }
            }
        }
    }
}

#[test]
fn malformed_catalog_aliases_do_not_panic() {
    for alias in ["", "😀", "@", "@scope/包"] {
        let mut value: Value = json!({"lockfileVersion":"9.0","importers":{".":{"dependencies":{"pkg":{"specifier":"catalog:","version":"1.0.0"}}}},"packages":{},"snapshots":{}});
        value["catalogs"] =
            json!({"default":{"pkg":{"specifier":format!("npm:{alias}"),"version":"1.0.0"}}});
        let _ = Lockfile::parse(&value.to_string()).unwrap().validate();
    }
}
