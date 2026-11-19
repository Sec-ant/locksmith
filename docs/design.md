# Locksmith architecture

**English** · [简体中文](design.zh-CN.md)

A pnpm lockfile records both the release of a package and the dependencies bound to each instance of that release. Locksmith uses those bindings to merge branch edits, reduce drift after an install, or repair a committed lock. For installation and commands you can run in your project, see the [README](../README.md).

## How a merge can use both branches

Suppose an app depends on `lib@1.0.0`, which gets its peers `a` and `b` from the app. The merged manifest allows any version of `a` and requires `b@2.0.0`. Both branches have a valid `a`, so the caller’s preference for theirs breaks the tie. Only ours has the requested `b`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/peer-selection-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="diagrams/peer-selection.svg">
  <img src="diagrams/peer-selection.svg" alt="The app selects a@3.0.0 from theirs and b@2.0.0 from ours. Both peers form the identity of its lib@1.0.0 instance.">
</picture>

The result has one `lib@1.0.0(a@3.0.0)(b@2.0.0)` snapshot. A **snapshot** records how an instance connects to its dependencies. Another app can use the same release of `lib` with different peers and get its own snapshot. This is why a branch preference applies to each dependency individually. [Test for this example](../crates/locksmith/tests/contextual_merge/workspace.rs)

## Where inputs come from

The CLI reads the lockfile, manifests, and workspace settings. The dashed arrows show when other inputs come into play. A command that refers to a commit uses Git, and an optional metadata lookup uses the registry. A Rust application can pass the same data directly to the library in memory.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="diagrams/architecture-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="diagrams/architecture.svg">
  <img src="diagrams/architecture.svg" alt="The CLI reads project files, uses Git for refs or a merge driver, and can request registry metadata. Both the CLI and a Rust application call the same in-memory library.">
</picture>

| CLI invocation | Inputs the CLI collects | Library operation |
| --- | --- | --- |
| `merge BASE OURS THEIRS` | Three files. With `--context FILE`, also the final manifests and workspace settings. | `merge` |
| `merge --git-driver %O %A %B %P` | Git’s three lockfiles and the workspace declarations from the merged tree. | `merge` |
| `converge REF` | Baseline lock and declarations at `REF`, installed lock and declarations in the working tree. | `converge` |
| `repair [REF]` | Lock and declarations at one commit, plus parent locks if needed. Defaults to `HEAD`. | `repair` |

During a rebase, the driver uses the replayed commit’s original parent as the base and the current rebased `HEAD` as ours. It checks that Git’s lockfiles match those revisions before using the merged workspace. For equally suitable pins it looks for the mainline side using `origin/HEAD`, or local `main` and `master` when that symbolic ref is absent. A mainline tip takes priority, followed by a commit on mainline history. Other ties keep ours.

## Rust library API

All three operations return `Result<Lockfile, LockfileError>`. On success, `Lockfile::to_yaml()` renders the result. Locksmith reuses unchanged source records and preserves the order of overrides, which affects their precedence.

### Merge branch edits

```rust
let merged = merge(&base, &ours, &theirs, Some(&merge_context))?;
```

Pass three parsed locks or three YAML inputs. Pass `None` as the fourth argument for a three-way comparison. A disagreement in the reachable graph then produces a conflict. With a context, the final declarations determine which pins are eligible. Locksmith follows each chosen pin through its dependencies and checks the consumer’s peers. If a choice fails, it can reconsider the implicated pins with the other branch preferred.

With `autoDedupe` enabled, compatible transitive versions introduced by different branches can share a release when every affected parent accepts it. A locked edge or a published parent declaration provides that evidence. A contextual merge of raw YAML can also recover duplicate snapshot entries when all interpretations give the same result.

### Converge after an install

```rust
let reduced = converge(&baseline, &installed, &manifest_changes, options)?;
```

Supply the before/after manifests for each change. If a range changes from `^1.0.0` to `>=1 <2` and the install chose `1.1.0`, the baseline pin `1.0.0` still fits. A dependency added in the same edit brings in its required graph.

For workspace rule edits, supply the old and new settings in `ConvergeOptions::baseline_configuration` and `configuration`. Other resolution settings, workspace membership patterns, hooks, and patches must agree across the inputs.

- **Catalogs** affect projects that reference the edited entry. The cached catalog version and a project’s pin can differ when both satisfy the range.
- **Overrides** apply by dependency name, parent, and declared range. Removing a rule takes the replacement edge from the installed lock. An exact override on a required package-extension dependency also applies when the rule itself stayed the same.
- **Package extensions** can change ordinary, optional, and peer dependencies. If one app upgrades its peer provider, another app can keep the original binding. When a cycle or shared instance would require incompatible bindings, convergence keeps the installed peer assignments on those paths. It can still restore unrelated child pins.

For an unchanged parent release with matching patch and peers, a locked instance provides evidence for its child pins. For an upgraded parent, published dependency ranges can justify reusing an earlier child after the release integrity and overrides have been checked.

### Repair a committed lock

```rust
let repaired = repair(yaml, &repair_context)?;
```

Repair starts with the current lock. It removes identical duplicate package or snapshot records, reconciles stale workspace declarations in the lock, and restores missing reachable records. For example, a package extension can require a dependency that an override pins exactly. Repair can restore that dependency when one locked instance matches the consumer’s peers. Different contents under the same duplicate key require a decision.

A healthy input comes back byte-for-byte. To try additional parent or registry evidence, call `RepairInput::parse(yaml)` once and reuse the result with `.apply(&context)`. Each attempt starts from the original input.

## Merge context

For `merge --context FILE`, supply JSON like the following. The root workspace project uses the key `"."`. A monorepo also includes each final workspace manifest under its path.

```json
{
  "target": "theirs",
  "manifests": { ".": { "dependencies": { "a": "*", "b": "2.0.0", "lib": "1.0.0" } } },
  "configurations": [{}, {}, {}],
  "configuration": {},
  "patches": {}
}
```

`configurations` contains the base, ours, and theirs settings in order. `configuration` holds the merged settings. `patches` maps each configured patch path to its file contents. The Git driver collects these inputs automatically. If a required project file is conflicted, it names the file and leaves the lock unmerged.

## Missing evidence and diagnostics

If the final manifest requires `a@4.0.0` and the locks only contain versions 1 through 3, the library can return `ResolutionRequired` with a `RegistryRequest::Version`. A `RegistryRequest::Manifest` asks for the dependency declarations of a known release. An opaque peer hash instead needs a combination of providers that reproduces it.

With `--allow-network`, the CLI fetches metadata from the project’s default or scoped registry, applies its credentials, release-age and version-selection settings, and caches the result for the command. If a project hook or trust policy requires more evidence, merge reports the unresolved input. For an optional convergence lookup that fails, it reports the failure and keeps the installed edge. Rust callers can supply `RegistryMetadata` through the operation’s context or options and retry the same inputs. pnpm handles package downloads and installation.

The contextual CLI also applies the project’s peer policy. With `strictPeerDependencies` enabled, unresolved peer issues reject the result. Otherwise the CLI reports the issue count. Rust callers can inspect the same issues with `inspect_peers`.

Standalone commands write YAML to stdout on success and diagnostics to stderr. Exit code `1` indicates a conflict, an unresolved dependency choice, or an unmet peer requirement. Other errors use `2`. The Git driver returns `1` on failure and gives Git the three original texts for a manual merge. Git keeps the lock unmerged even when the text merges cleanly, so the developer can review the reported dependency.
