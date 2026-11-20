---
default: patch
---

Fix `repair` rejecting empty pnpm rules and reordered `ignoredOptionalDependencies`. Omit empty overrides and patch maps in contextual merges.

Fix `converge` restoring an invalid baseline when the manifests are unchanged.

Allow `converge` when a workspace member changes its own `packageManager` declaration.
