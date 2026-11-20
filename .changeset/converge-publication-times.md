---
default: patch
---

Fix `converge` retaining publication times for versions that are no longer direct dependencies. Preserve pnpm 9 and 10 publication times for retained direct dependencies in `merge --context`.
