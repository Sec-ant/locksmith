use locksmith::{Lockfile, Value, WorkspaceConfig};
use serde_json::json;

mod extensions;
mod identity;
mod overrides;
mod patches;
mod policy;
mod providers;
mod workspace;

fn lock(a: u8, b: u8, cycle: bool) -> Lockfile {
    let suffix = format!("(a@{a}.0.0)(b@{b}.0.0)");
    let mut value = json!({
        "lockfileVersion":"9.0", "settings":{"autoInstallPeers":true,"excludeLinksFromLockfile":false},
        "importers":{".":{"dependencies":{
            "a":{"specifier":"*","version":format!("{a}.0.0")},
            "b":{"specifier":"*","version":format!("{b}.0.0")},
            "lib":{"specifier":"1.0.0","version":format!("1.0.0{suffix}")}
        }}},
        "packages":{
            format!("a@{a}.0.0"): {"resolution":{"integrity":"sha512-a"}},
            format!("b@{b}.0.0"): {"resolution":{"integrity":"sha512-b"}},
            "lib@1.0.0":{"resolution":{"integrity":"sha512-lib"},"peerDependencies":{"a":"*","b":"*"}}
        },
        "snapshots":{
            format!("a@{a}.0.0"): {},format!("b@{b}.0.0"): {},
            format!("lib@1.0.0{suffix}"): {"dependencies":{"a":format!("{a}.0.0"),"b":format!("{b}.0.0")}}
        }
    });
    if cycle {
        value["packages"]["wrapper@1.0.0"] = json!({"resolution":{"integrity":"sha512-wrapper"}});
        value["snapshots"][format!("lib@1.0.0{suffix}")]["dependencies"]["wrapper"] =
            json!(format!("1.0.0{suffix}"));
        value["snapshots"][format!("wrapper@1.0.0{suffix}")] = json!({"dependencies":{"lib":format!("1.0.0{suffix}")},"transitivePeerDependencies":["a","b"]});
    }
    Lockfile::parse(&value.to_string()).unwrap()
}

fn cfg(value: &Value) -> WorkspaceConfig {
    WorkspaceConfig::from_pnpm(value).unwrap()
}
