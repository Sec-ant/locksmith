---
default: patch
---

# Accept more pnpm workspace lockfiles

Accept local catalog links with an omitted or range-valued catalog entry, honor the dependency group pnpm records for an overridden peer also declared as a development dependency, and recognize pnpm 12 hook checksums sorted by path. Workspace commands honor pnpm 9 and 10 legacy `resolutions` and `ignore-pnpmfile` settings, and registry lookup accepts boolean deprecation metadata.
