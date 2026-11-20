## 0.2.1 (2026-11-20)

### Fixes

#### Publish the Rust library as `es-locksmith`

Add an MIT license and publish the library on crates.io. Rust code imports it as `locksmith`.

## 0.2.0 (2026-11-20)

### Breaking Changes

#### Replace automatic `main` and `master` fallback with configurable mainline selection

The Git merge driver uses `origin/HEAD` by default. Set `locksmith.mainline` to use a different Git ref. On repositories without either setting, equally suitable lockfile candidates now keep `ours`. If your repository previously relied on a local `main` or `master` branch being detected automatically, set `locksmith.mainline` to that branch explicitly.
