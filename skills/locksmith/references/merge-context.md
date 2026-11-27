# Build merge context

Run `locksmith merge --context` from the affected pnpm workspace root so registry resolution can use its `.npmrc`. Use the same base, ours, and theirs revisions for both the lockfiles and the source settings.

- For an ordinary two-branch merge, use the common ancestor, `HEAD`, and the incoming tip. In a paused merge, the incoming tip is `MERGE_HEAD`.
- For a paused rebase replaying a single-parent commit, use `REBASE_HEAD^`, `HEAD`, and `REBASE_HEAD`. In a rebase, stage 2 is the branch being rebased onto plus commits already replayed, and stage 3 is the commit being replayed.
- Take final inputs from the resolved worktree. Include the root `package.json` and each final workspace member's `package.json`, keyed by its path relative to the workspace root. Include the root manifest's `packageManager` when present.

Use this JSON shape for a single-package project with no pnpm settings or patches. Replace the sample manifest and use the actual settings from each revision.

```json
{
  "target": "ours",
  "manifests": {
    ".": { "dependencies": { "is-number": "7.0.0" } }
  },
  "configurations": [{}, {}, {}],
  "configuration": {},
  "patches": {}
}
```

`target` is `ours` or `theirs` and breaks ties between admissible pins. Use the intended mainline side when known. The `configurations` array is ordered base, ours, theirs. During a pnpm upgrade, copy each revision’s root `packageManager` string into its entry so Locksmith can select peer bindings from the matching pnpm version. Each entry and the final `configuration` hold effective pnpm settings, including catalogs, overrides (merging top-level `resolutions` where supported), and package extensions where present. The Git driver starts with `package.json`'s `pnpm` object and `resolutions`, applies recognized legacy `.npmrc` settings for pnpm <11 (or an unspecified version), then overlays `pnpm-workspace.yaml` (filtering keys not supported by the selected pnpm version). Keep registry credentials in the project's normal pnpm auth configuration.

For patched dependencies, include the final `patchedDependencies` settings and map each configured patch path to its final file contents in `patches`. For custom `.pnpmfile` hooks, the driver records hook contents by path in `pnpmfileChecksum`. See the [CLI workspace reader](https://github.com/Sec-ant/locksmith/blob/main/crates/locksmith-cli/src/workspace.rs) for exact settings handling when those inputs affect resolution.
