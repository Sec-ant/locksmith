---
default: patch
---

Keep the final workspace’s configuration dependencies and package-manager pin when merging or converging bootstrap locks. The Git driver, CLI, and Node API interpret the final settings with the pnpm version from the selected bootstrap graph when the manifest leaves a range or omits the version. Repair checks each parent using its recorded pnpm version when recovering missing records. Repair and workspace validation honor `devEngines.packageManager`, `packageManager`, and `pmOnFail`, including when a frozen install requires a bootstrap pin.
