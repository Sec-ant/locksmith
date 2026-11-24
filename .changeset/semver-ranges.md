---
default: minor
---

# Match npm range semantics for partial upper bounds

Version ranges such as `<=27` or `>=24 <=27` now include every 27.x release, as in npm and pnpm. Rust APIs that take or return a pnpm version, including `WorkspaceConfig::from_pnpm_version` and `Lockfile::recorded_pnpm_version`, use `nodejs_semver::Version` 7 in place of `node_semver::Version` 2.
