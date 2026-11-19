# Locksmith architecture

The `locksmith` Rust crate implements `merge`, `repair`, and `converge`. All three operate on lockfiles and project data in memory. The CLI collects the inputs for each operation and writes the resulting lockfile.

## CLI commands

- **Merge two lockfile revisions.** `locksmith merge BASE OURS THEIRS` runs the three-way merge outside Git. Add `--context FILE` to run a contextual merge. The JSON object supplies `target` (`"ours"` or `"theirs"`), `manifests` keyed by importer path, `configurations` in base/ours/theirs order, the final `configuration`, and `patches` keyed by configured path.
- **Git merge driver.** During a two-head merge or a rebase, Git can invoke the driver before updating the working tree. For a replayed commit, the driver uses its original parent as the base and the current rebased HEAD as ours. The driver verifies the three lock inputs against Git history and reads the merged tree for manifests, workspace settings, and patches. A conflict in a required file leaves the lock unmerged and the diagnostic names the path. Unrelated source conflicts can coexist with a successful lock merge. When pins are equally suitable, the driver prefers the side identified as mainline using the local and remote default-branch refs named by `origin/HEAD`. Without that symbolic ref, it checks local `main` and `master`. An exact mainline tip takes priority, followed by a side in mainline history. Other ties retain ours (the accumulated result during rebase). On success the driver writes the validated lockfile. On refusal it reports the cause and asks Git to merge the original three texts for manual editing. Even a clean text merge leaves the index unmerged so the developer can review the dependency decision.
- **Repair a committed lock.** `locksmith repair [REF]` reads the lock and final workspace from the same commit, defaulting to `HEAD`. Parent locks and optional registry metadata can supply missing records.
- **Reduce changes after an install.** `locksmith converge REF` reads the baseline lock and manifests from Git and the installed candidate from the working tree. Workspace configuration must match the baseline. The library follows changed declarations through local dependencies and peer consumers to determine which candidate records belong in the result.

For scripts, all three commands write YAML to stdout and diagnostics to stderr. Exit code 0 means success, 1 indicates an unresolved conflict or peer requirement, and 2 indicates another error. The Git driver returns 1 on failure so Git retains the unmerged stages.

## From a command to a library call

![File merges and Git driver merges call the same Rust merge function. Repair and convergence each call the corresponding Rust function.](diagrams/architecture.svg)

The CLI supplies the fourth argument to `merge`. A plain file merge passes `None`. With `--context`, the CLI reads workspace intent from JSON. The Git driver collects the same information from the merged tree. Both paths and `converge` accept `--allow-network`. The CLI starts the registry client only after the core requests a specific release or declaration. Plain three-file merges use only their three inputs.

## Rust library API

**Merge two branches** with `merge(base, ours, theirs, context)`. Pass parsed lockfiles or YAML slices. The context determines how Locksmith handles competing changes.

```rust
let merged = merge(&base, &ours, &theirs, None)?;
let merged = merge(&base, &ours, &theirs, Some(&context))?;
```

With `None`, Locksmith compares both branches with the ancestor and reports reachable disagreements. With `Some(&context)`, final manifests and settings can settle a disagreement. For example, if the branches pin versions 2 and 3 and the merged manifest requires version 2, Locksmith can select version 2. A contextual merge of raw YAML can also recover duplicate snapshot entries when every interpretation agrees on the result.

**Repair a committed lock** with `repair(yaml, &context)`. `RepairContext` supplies the final manifests, workspace configuration, patches, and available evidence. Repair starts from the current pins. It removes identical duplicate package or snapshot records, reconciles stale importer declarations, and restores missing records reachable from those importers. A replacement peer instance must fit the retained providers. Two catalog pins may differ when both satisfy the configured range. Conflicting duplicate contents, missing workspace intent, and configuration changes whose effects cannot be isolated produce diagnostics.

**Reduce changes after an install** with `converge(baseline, candidate, changes, options)`. Supply both parsed locks and the before/after manifests. Locksmith keeps a baseline pin when the new range still accepts it. For example, changing `^1.0.0` to `>=1 <2` can keep `1.0.0` even if the install chose `1.1.0`. Adding a dependency in the same edit still brings in its required graph.

When a `packageExtensions` rule changes, pass the effective settings at the ref in `ConvergeOptions::baseline_configuration` and the installed settings in `configuration`. For ordinary and optional dependencies, Locksmith follows paths to the packages matched by the edit. It takes the required dependency from the installed lock and keeps the other baseline bindings. A new peer binding propagates up a path until a package already supplies it. If the binding changes an instance's identity, the result must match an identity in one of the input locks, including its peer hash. Ambiguous paths produce a diagnostic naming the package.

When a dependency keeps the same release and patch, Locksmith retains its peer bindings in each workspace project unless that project's providers change. For example, upgrading React in one app can rebind a plugin to the new React version while leaving its other optional peers untouched. Another app that still uses the old React keeps its plugin instance. Projects inherit providers from the workspace root when `resolvePeersFromWorkspaceRoot` is enabled.

A locked instance of the same parent, with the same patch and peer bindings, provides evidence for its earlier child pins. For a new parent release, Locksmith can use registry metadata to check its dependency ranges. It verifies the distribution integrity against the installed lock and applies overrides before reusing a baseline child. The final dependency paths determine whether a shared snapshot is optional.

All three operations return `Result<Lockfile, LockfileError>`. The caller can inspect peer diagnostics with `inspect_peers` and serialize the result with `Lockfile::to_yaml()`.

## How a contextual merge selects dependencies

1. **Establish the final requirements.** The caller supplies every final workspace manifest, including the root, the base/ours/theirs workspace configurations, the final configuration, and any configured patch contents. Together these describe what each project requests and which settings produced each input lock.
2. **Choose direct dependencies.** The final manifest and effective configuration decide which pins are eligible. The target branch supplies the tie-break for each dependency, including when an unrelated configuration change occurred elsewhere. A branch edit that excludes the mainline pin selects a different release. Each selected subgraph must also fit the final configuration and peer providers.
3. **Build the reachable instances.** A package record describes a release. A snapshot describes an instance of that release and its dependencies. When a peer provider changes, consumers of that peer may need a different snapshot even if their own release stays the same. Locksmith follows those dependencies and rebinds affected instances. If a choice fails, it retries the implicated pins with the other branch preferred.
4. **Reconcile versions brought together by the merge.** In a workspace with `autoDedupe` enabled, different branches can introduce compatible versions of the same transitive dependency. Locksmith reuses a single ordinary version when every affected parent accepts it. A matching locked edge or the published parent declaration supplies the evidence. Direct pins and peer bindings retain their own selection rules.
5. **Validate the result.** Locksmith checks the assembled graph and its importer declarations. When rendering YAML, it reuses unchanged records and preserves the order of overrides, which can affect precedence.

### How one lockfile gets pins from both branches

Suppose the final root manifest requests `a: "*"`, `b: "2.0.0"`, and `lib: "1.0.0"`. The library `lib` has peers `a` and `b`. Both branches use the same settings, and the caller prefers theirs.

| Dependency | Ours | Theirs | Selected |
| --- | --- | --- | --- |
| `a` | 2.0.0 | 3.0.0 | 3.0.0 from theirs. Both pins satisfy `*`. |
| `b` | 2.0.0 | 1.0.0 | 2.0.0 from ours. The manifest requires this version. |

![The merged instance of lib uses peer a at 3.0.0 from theirs and peer b at 2.0.0 from ours.](diagrams/peer-selection.svg)

The resulting snapshot is `lib@1.0.0(a@3.0.0)(b@2.0.0)`. The release stays at 1.0.0, but its identity records the pair of providers selected for this workspace. The [contextual merge test](../crates/locksmith/tests/contextual_merge/workspace.rs) exercises this example.

### When a merge needs more information

If the final manifest requires `a@4.0.0` and all three locks contain only versions 1 through 3, the library returns `ResolutionRequired` with the dependency path and a `RegistryRequest::Version`. A `RegistryRequest::Manifest` asks for the original dependency declarations of a release already in a lock. A hash in a snapshot identity requires different evidence. Locksmith looks for workspace providers that reproduce that hash.

With `--allow-network`, the CLI requests the package’s registry metadata, caches it for the command, and passes the selected release back to the same pure operation. Locked transitive dependencies take precedence. A new release can therefore use different peer instances in different importers. The only network reads are registry metadata requests. Tarball URLs and integrity hashes in the output describe what a later pnpm install would fetch.

The client supports default and scoped registry routes, scoped credentials, release-age settings, and highest or lowest-direct version selection. When a project hook can rewrite a new dependency, or an active trust policy needs additional evidence, a merge reports the unresolved input. A missing manifest, catalog declaration, or patch still requires a project file. If an optional lookup fails during convergence, Locksmith reports the lookup and keeps the installed edge.

A Rust caller can supply a `RegistryMetadata` via `MergeContext::registry` or `ConvergeOptions::registry`. On a typed request, the caller selects a version under its registry policy, calls `RegistryMetadata::insert`, and retries with the same locks. The library performs no I/O.

Peer diagnostics are a separate decision. A caller can inspect the assembled lockfile with `inspect_peers`. The contextual CLI checks the project’s peer rules. With `strictPeerDependencies` enabled, outstanding issues reject the result. With it disabled, the CLI reports the issue count and accepts the lockfile.
