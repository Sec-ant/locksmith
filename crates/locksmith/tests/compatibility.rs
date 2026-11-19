//! Fixtures were produced by pnpm 9.15.9, 10.34.9, 11.28.12, and 12.21.0.
//! Base depends on is-number@7.0.0 and is-odd@3.0.1. Ours adds is-even@1.0.0;
//! theirs adds is-buffer@2.0.5. The assertions follow these manifest changes.
use locksmith::{Lockfile, merge};

macro_rules! compatibility {
    ($name:ident, $version:literal) => {
        #[test]
        fn $name() {
            let base_text = include_str!(concat!("fixtures/", $version, "/base.yaml"));
            let base = Lockfile::parse(base_text).unwrap();
            let ours = Lockfile::parse(include_str!(concat!("fixtures/", $version, "/ours.yaml")))
                .unwrap();
            let theirs =
                Lockfile::parse(include_str!(concat!("fixtures/", $version, "/theirs.yaml")))
                    .unwrap();
            for lockfile in [&base, &ours, &theirs] {
                lockfile.validate().unwrap();
            }
            assert_eq!(base.to_yaml().unwrap(), base_text);
            let merged = merge(&base, &ours, &theirs, None).unwrap();
            let project = merged.project().unwrap();
            let deps = project["importers"]["."]["dependencies"]
                .as_object()
                .unwrap();
            assert_eq!(deps.len(), 4);
            for (name, version) in [
                ("is-number", "7.0.0"),
                ("is-odd", "3.0.1"),
                ("is-even", "1.0.0"),
                ("is-buffer", "2.0.5"),
            ] {
                assert_eq!(deps[name]["version"], version);
                assert!(
                    project["snapshots"]
                        .get(format!("{name}@{version}"))
                        .is_some()
                );
            }
            let rendered = merged.to_yaml().unwrap();
            let roundtrip = Lockfile::parse(&rendered).unwrap();
            roundtrip.validate().unwrap();
            assert_eq!(roundtrip.project(), merged.project());
        }
    };
}

compatibility!(pnpm_9, "pnpm9");
compatibility!(pnpm_10, "pnpm10");
compatibility!(pnpm_11, "pnpm11");
compatibility!(pnpm_12, "pnpm12");
