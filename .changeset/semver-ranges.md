---
default: minor
---

# Match npm range semantics for partial upper bounds

Version ranges such as `<=27` or `>=24 <=27` now include every 27.x release, as in npm and pnpm. Rust callers that pass a pnpm version, for example to `WorkspaceConfig::from_pnpm_version` or `GenerateContext::with_pnpm_version`, now use `nodejs_semver::Version` 7 instead of `node_semver::Version` 2.
