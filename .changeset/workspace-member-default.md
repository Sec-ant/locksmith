---
default: patch
---

Use the root-only workspace default when `pnpm-workspace.yaml` omits `packages`, including when the pnpm version is unknown. An explicitly declared or recorded pnpm version before 10.5 retains its recursive default for an empty workspace configuration.
