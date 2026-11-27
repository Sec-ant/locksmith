---
default: minor
---

Replace `converge_with_patches` and `converge_with_recorded_baselines` with `converge` and `ConvergeOptions::patches` or `ConvergeOptions::recorded_baselines`, respectively. Rename `PeerVersionMatching::ReleaseCompatible` to `Pnpm10`, add `#[non_exhaustive]`, `#[must_use]`, `Display`, and standard Rust traits, and declare the default export and `CLI_FAILED` error variant in `npm/index.d.ts`.
