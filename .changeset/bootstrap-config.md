---
default: patch
---

Keep the final workspace’s configuration dependencies and package-manager pin when merging or converging bootstrap locks. The CLI and Node API infer the pnpm version from the manifest and the bootstrap lock. Repair and workspace validation honor `devEngines.packageManager`, `packageManager`, and `pmOnFail`, including when a frozen install requires a bootstrap pin.
