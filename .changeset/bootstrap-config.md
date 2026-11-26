---
default: patch
---

Keep the final workspace’s configuration dependencies and package-manager pin when merging or converging bootstrap locks. Repair and workspace validation honor pnpm’s `devEngines.packageManager`, `packageManager`, and `pmOnFail` rules, including when a frozen install requires a bootstrap pin.
