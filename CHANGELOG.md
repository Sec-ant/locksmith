## 0.3.8 (2026-11-26)

### Fixes

- Keep the final workspace’s configuration dependencies and package-manager pin when merging or converging bootstrap locks. The Git driver, CLI, and Node API interpret the final settings with the pnpm version from the selected bootstrap graph when the manifest leaves a range or omits the version. Repair checks each parent using its recorded pnpm version when recovering missing records. Repair and workspace validation honor `devEngines.packageManager`, `packageManager`, and `pmOnFail`, including when a frozen install requires a bootstrap pin.
- Resolve linked workspace members and manifests against the selected Git revision, including relative targets containing `.` and `..`. On Git 2.38 and later, blobless clones with one promisor remote fetch the selected missing manifests in a single batch. Convergence checks working directory links against the installed lockfile. A conflict in a linked manifest keeps the lockfile unmerged, including when its target filename contains non-UTF-8 bytes.
- Ignore unrelated non-UTF-8 filenames when discovering Git workspace members and dependency conflicts.
- Read peer provider manifests inside the workspace even when their directories are not workspace members.
- Report invalid lockfile setting types and compare quoted booleans under the selected pnpm version.
- Resolve the installed npm README’s links to type declarations and repository documentation.
- Validate the patch hash selected for each locked package version.
- Use the same default pnpm version policy for the Node `inspect` and `inspectPeers` APIs.
- Converge minor and patch pnpm upgrades using the final resolver configuration.
- Batch independent missing workspace and transitive dependency versions during contextual merges with registry access. Reuse a compatible dependency or alias before requesting an auto-installed peer.
- Preserve unreachable package extension records during `repair`.

## 0.3.7 (2026-11-25)

### Fixes

- Speed up `inspect` on large workspaces with links to directories outside the recorded graph.
- Check the effective `projectScopedOptionalPeers` header in `validate --workspace` for pnpm versions that record it.
- Preserve edits and removals of Git-ignored workspace members in `converge`, including their effective dependency specifiers, unchanged peer pins, dependency metadata, publish directories, and `devEngines`.
- Honor version-specific `ignoredOptionalDependencies` rules in `converge`, `repair`, and contextual merges, including negated patterns and sorted lockfile headers.
- Preserve override precedence when one branch reorders rules and the other inserts rules. Report incompatible orderings during three-way merges.
- Update snapshot optionality when merging branches adds or removes a required dependency path.
- Apply optional dependency exclusion patterns when reconstructing snapshots from package extensions or registry metadata. Required extension dependencies remain available after an override is removed.
- Ignore legacy `package.json#pnpm` settings in pnpm 11 and 12 workspaces, including when the bootstrap lock supplies the version or the caller supplies one unchanged configuration to `converge`.

## 0.3.6 (2026-11-25)

### Features

- Detect outdated workspace members and dependency declarations with `locksmith validate --workspace` without installing dependencies.

### Fixes

- Use the pnpm version recorded in the lockfile to load workspace configuration for `repair` when `packageManager` is absent.
- Use the root-only workspace default when `pnpm-workspace.yaml` omits `packages`, including when the pnpm version is unknown. An explicitly declared or recorded pnpm version before 10.5 retains its recursive default for an empty workspace configuration.

## 0.3.5 (2026-11-24)

### Features

- Match npm partial upper bounds such as `<=27`, which include every 27.x release. Rust APIs that take or return a pnpm version now use `nodejs_semver::Version` v7.
- Check a pnpm lockfile for structural errors such as duplicate keys and missing snapshots with `locksmith validate` without installing dependencies.

### Fixes

- Match pnpm's peer declarations for newly added releases. Omit self-peers with pnpm 9.15.1 and later, retain unparseable peer ranges, and record metadata-only optional peers as `*`, including when an ignored optional dependency has the same name.
- Honor `ignore-pnpmfile` and exclude ignored hooks from workspace change and conflict checks.
- Read legacy `resolutions` alongside pnpm overrides in pnpm 9 and 10 workspaces.
- Preserve pnpm's version-specific dependency group for local overrides of dependencies declared as both development dependencies and peers.
- Accept omitted or range-valued catalog entries for linked workspace packages.
- Recognize pnpm 12 hook checksums sorted by path.
- Accept boolean deprecation metadata from registries when selecting published versions.
- Resolve conflicting duplicate snapshots in a committed lockfile with `repair` when a three-way merge of its parents identifies an existing copy to keep.

## 0.3.4 (2026-11-22)

### Features

- Inspect peer bindings, their workspace paths, and packages installed in multiple versions or instances with `locksmith inspect` and the Rust and Node.js APIs.
- Expose lockfile merging, convergence, repair, and inspection to Node.js and distribute the Locksmith CLI through the `es-locksmith` npm package.

### Fixes

- Follow workspace links from package snapshots when tracing dependency closures.
- Validate peer ranges and project exceptions against locked provider versions, including prereleases and local packages, and reject malformed peer declarations.

## 0.3.3 (2026-11-21)

### Fixes

- Accept catalog dependencies linked to matching local workspace packages.
- Reject repairs and convergence when the lockfile records incompatible workspace injection, peer suffix length, or missing peer types settings.
- Preserve hook-derived dependencies by refusing ambiguous importer repairs and selecting pnpm’s active default hook.
- Read BOM-prefixed JSON, JSON5, and YAML package manifests in pnpm’s search order.
- Fetch missing workspace manifests in batches when reading a partial Git clone.
- Keep pnpm workspace members matched by normalized paths and brace patterns.

## 0.3.2 (2026-11-21)

### Fixes

- Reject an invalid baseline in `converge` even when dependency declarations are unchanged.
- Allow `converge` when a workspace member changes its own `packageManager` declaration.
- Drop publication times for versions that are no longer direct dependencies after `converge`.
- Accept valid lockfiles in `repair` when pnpm rules are empty or `ignoredOptionalDependencies` is reordered.
- Omit empty `overrides` and `patchedDependencies` maps from `merge --context` output.
- Preserve pnpm 9 and 10 publication times for direct dependencies retained by `merge --context`.

## 0.3.1 (2026-11-20)

### Fixes

- Show `--help` output when `locksmith` runs without arguments.
- Speed up `converge` in large workspaces by recovering peer bindings on demand and limiting Git's search for untracked workspace manifests.

## 0.3.0 (2026-11-20)

### Breaking Changes

- Require `pnpm_version` in Rust `WorkspaceConfig` literals that specify every field (`None` if the version is unknown).

### Features

- Honor each pnpm version’s precedence for overlapping `overrides` in `converge` so upgrades retain compatible baseline versions and peer bindings.

### Fixes

- Keep a patched package’s locked dependency versions when `converge` updates its patch hash during a pnpm upgrade.
- Prefer peer bindings from the branch using the final `packageManager` version when `merge` combines lockfiles from different pnpm versions.

## 0.2.1 (2026-11-20)

### Fixes

#### Publish the Rust library as `es-locksmith`

Add an MIT license and publish the library on crates.io. Rust code imports it as `locksmith`.

## 0.2.0 (2026-11-20)

### Breaking Changes

#### Replace automatic `main` and `master` fallback with configurable mainline selection

The Git merge driver uses `origin/HEAD` by default. Set `locksmith.mainline` to use a different Git ref. On repositories without either setting, equally suitable lockfile candidates now keep `ours`. If your repository previously relied on a local `main` or `master` branch being detected automatically, set `locksmith.mainline` to that branch explicitly.
