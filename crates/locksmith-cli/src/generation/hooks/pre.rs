use crate::Result;
use locksmith::{GenerateContext, Lockfile};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

pub(super) fn context(previous: &Value, input: &GenerateContext<'_>) -> Result<Value> {
    let major = input
        .pnpm_version
        .ok_or("select a pnpm version to evaluate preResolution")?
        .major();
    let cfg = input.configuration;
    let root = env::current_dir()?;
    let modules = root.join(cfg["modulesDir"].as_str().unwrap_or("node_modules"));
    let virtual_store = cfg["virtualStoreDir"].as_str().filter(|_| major <= 10);
    let current_path = virtual_store
        .map_or_else(|| modules.join(".pnpm"), |path| root.join(path))
        .join("lock.yaml");
    // Like pnpm, warn and continue without an unreadable installed-state lockfile.
    let current = match fs::read_to_string(&current_path) {
        Ok(raw) => match Lockfile::try_from(raw) {
            Ok(lock) => Some(lock),
            Err(e) => {
                eprintln!("warning: ignoring {}: {e}", current_path.display());
                None
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", current_path.display()).into()),
    };
    let mut settings = json!({"autoInstallPeers":cfg["autoInstallPeers"].as_bool().unwrap_or(true),
        "excludeLinksFromLockfile":cfg["excludeLinksFromLockfile"].as_bool().unwrap_or(false)});
    if major >= 10 {
        settings["peersSuffixMaxLength"] =
            cfg["peersSuffixMaxLength"].as_u64().unwrap_or(1000).into();
    }
    Ok(
        json!({"modern":major >= 12, "current":current.as_ref().and_then(Lockfile::project), "wanted":previous,
        "importers":input.manifests.keys().collect::<Vec<_>>(), "settings":settings, "lockfileDir":root,
        "storeDir":store_path(&root,cfg,major)?, "registries":crate::registry::hook_registries(cfg,major)?}),
    )
}

fn store_path(root: &Path, config: &Value, major: u64) -> Result<PathBuf> {
    let version = match major {
        9 => "v3",
        10 => "v10",
        _ => "v11",
    };
    let configured = [
        "pnpm_config_store_dir",
        "PNPM_CONFIG_STORE_DIR",
        "npm_config_store_dir",
        "NPM_CONFIG_STORE_DIR",
    ]
    .into_iter()
    .find_map(env::var_os)
    .map(PathBuf::from)
    .or_else(|| config["storeDir"].as_str().map(PathBuf::from));
    let home = env::home_dir();
    let store = if let Some(path) = configured {
        if let Ok(relative) = path.strip_prefix("~") {
            home.ok_or("cannot expand storeDir without a home directory")?
                .join(relative)
        } else {
            root.join(path)
        }
    } else {
        let pnpm_home = env::var_os("PNPM_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_DATA_HOME").map(|p| PathBuf::from(p).join("pnpm")))
            .or_else(|| {
                if cfg!(windows) {
                    env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("pnpm"))
                } else {
                    home.map(|h| {
                        h.join(if cfg!(target_os = "macos") {
                            "Library/pnpm"
                        } else {
                            ".local/share/pnpm"
                        })
                    })
                }
            })
            .ok_or("cannot locate the pnpm store directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let device = root.metadata()?.dev();
            if let Some(parent) = pnpm_home.ancestors().find(|p| p.exists())
                && parent.metadata()?.dev() != device
            {
                let mut mount = root;
                while let Some(parent) = mount.parent() {
                    if parent.metadata()?.dev() != device {
                        break;
                    }
                    mount = parent;
                }
                return Ok(mount.join(".pnpm-store").join(version));
            }
        }
        pnpm_home.join("store")
    };
    Ok(if store.ends_with(version) {
        store
    } else {
        store.join(version)
    })
}
