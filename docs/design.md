# How Locksmith works

This document explains the problem Locksmith solves, how a pnpm v9 lockfile represents a dependency graph, and how `merge`, `converge`, and `repair` change that graph. It assumes you have used pnpm but have not read `pnpm-lock.yaml` closely. Installation and command usage are in the [README](../README.md).

## 1. Why a lockfile is not a text file

Git merges `pnpm-lock.yaml` as YAML text. A three-way text merge can produce a file that parses but no longer describes a valid dependency graph:

- **Duplicate snapshots.** Both branches add the same snapshot key with different contents. YAML permits the duplicate; pnpm does not.
- **Stale importer declarations.** A manifest changes, but the importer still points to the old version.
- **Missing records.** The text merge drops a package or snapshot that another record still references.
- **Wrong peer bindings.** A snapshot key encodes its peer providers. Text can keep a snapshot whose key and body no longer match the consumers that selected it.

The usual workaround is to run `pnpm install` after the conflict and let pnpm regenerate the lockfile. The result is valid, but pnpm re-resolves the whole graph from the manifests:

- Unrelated versions can move because their ranges allow newer releases.
- Peer bindings can change when a provider is re-resolved.
- The command needs registry metadata unless everything is cached.
- Large workspaces pay the full resolution cost.

Lockfile diffs are rarely reviewed, so these changes enter the branch silently.

Locksmith takes a third path. It reads the lockfile as a graph, uses the final manifests and workspace settings as constraints, and keeps every record those constraints still accept. It queries the registry only when the lockfiles do not contain enough evidence. The rest of this document describes that model and the three operations built on it.

## 2. The data model

A v9 lockfile is a graph in three layers. Importers point into snapshots, snapshots point to other snapshots through their dependency edges, and `packages` holds the metadata shared by snapshots. pnpm 11 and later also write a separate environment document for package-manager dependencies; the project document below is the main input for Locksmith.

### Importers

`importers` has one entry per workspace project. The root project uses the key `"."`. Each dependency entry has two fields:

- `specifier`: the range written in `package.json` (for example `^1.0.0` or `catalog:default`).
- `version`: the locked reference that pnpm selected (for example `1.0.0` or `1.0.0(a@3.0.0)(b@2.0.0)`).

### Packages

`packages` holds release metadata, keyed by `name@version` (or by a locator for git, file, and tarball dependencies):

- `resolution` and `integrity`: the artifact identity.
- `engines`, `os`, `cpu`, `libc`, `bin`, and similar install metadata.
- `peerDependencies` and `peerDependenciesMeta`: what the release expects its consumer to provide.

A release has one `packages` record no matter how many times it is installed.

### Snapshots

`snapshots` holds instances. The key is `name@version`, followed by one parenthesized entry per peer provider:

```
lib@1.0.0(a@3.0.0)(b@2.0.0)
```

The body records the instance's edges: `dependencies`, `optionalDependencies`, and sometimes `transitivePeerDependencies`. The same release can have several snapshots when different consumers provide different peers. This is why `lib@1.0.0` can appear as `lib@1.0.0(a@2.0.0)(b@1.0.0)` in one project and `lib@1.0.0(a@3.0.0)(b@2.0.0)` in another. The `packages` record is shared; the `snapshots` records are not.

The peer suffix is part of the identity, not decoration. Two snapshots with the same release but different providers have different dependency edges and can resolve different child versions. Merging them as if they were the same record loses information.

### Workspace rules

Several top-level fields change which pins (locked versions) are valid:

- `settings`: `autoInstallPeers`, `dedupePeers`, and related resolution switches.
- `catalogs`: named version ranges referenced by `catalog:` specifiers.
- `overrides`: replacement rules keyed by dependency name, parent, and range. Their order matters when several rules match.
- `patchedDependencies`: patch files applied to specific releases.
- `packageExtensionsChecksum`, `pnpmfileChecksum`, `ignoredOptionalDependencies`: checksums and settings that must match the workspace configuration.

Package extensions and workspace settings live in the workspace configuration; the lockfile stores their effects or checksums.

### A minimal example

```yaml
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      a:
        specifier: '*'
        version: 3.0.0
      b:
        specifier: 2.0.0
        version: 2.0.0
      lib:
        specifier: 1.0.0
        version: 1.0.0(a@3.0.0)(b@2.0.0)

packages:
  a@3.0.0:
    resolution: {integrity: sha512-a}
  b@2.0.0:
    resolution: {integrity: sha512-b}
  lib@1.0.0:
    resolution: {integrity: sha512-lib}
    peerDependencies:
      a: '*'
      b: '*'

snapshots:
  a@3.0.0: {}
  b@2.0.0: {}
  lib@1.0.0(a@3.0.0)(b@2.0.0):
    dependencies:
      a: 3.0.0
      b: 2.0.0
```

The root project selects `a@3.0.0`, `b@2.0.0`, and one instance of `lib@1.0.0`. The `lib` release declares `a` and `b` as peers; the snapshot records that this instance receives `a@3.0.0` and `b@2.0.0`. Another project could use the same `lib@1.0.0` release with `a@2.0.0` and get a second snapshot.

Reachability matters. Only importers are entry points. A package or snapshot that no importer can reach is dead weight, and a missing record on a reachable path makes the lockfile invalid. Locksmith's operations preserve reachable records and prune the rest.

## 3. merge

`merge` combines a common ancestor with two branch locks. It has two modes: a contextual merge driven by the final workspace declarations, and a three-way comparison when no declarations are available. The Git driver uses the contextual mode.

### Inputs

- Three lockfiles: `base`, `ours`, `theirs`.
- The final manifests for every workspace project, including `"."`.
- The effective workspace configuration for base, ours, theirs, and the final merged state.
- Patch contents for the configured patch paths.
- Optional registry metadata.

The library receives all of these in memory. The CLI collects them from files, Git, and the workspace.

### Selecting against the final workspace

The contextual merge treats the final manifests as the source of truth:

1. For each importer dependency, collect the locked candidates from the three inputs whose versions satisfy the final manifest range.
2. Prefer candidates from sources whose resolution settings match the final configuration, ordered by the target branch. Other sources supply individually compatible records when the preferred sources lack a suitable candidate. Each dependency is selected separately, so one result can contain pins from both branches.
3. Follow each selected record through its dependencies. A candidate is eligible only if its peer context can be satisfied by the consumer's selected dependencies.
4. Assemble the reachable graph: copy the selected snapshots and packages, rebind peers to the selected providers, apply overrides and patches, and apply package extensions.
5. Validate the result. A missing version or release declaration produces `ResolutionRequired` with a `RegistryRequest`; the caller can supply metadata and retry.

When several locked candidates are equally suitable, the target branch breaks the tie. The Git driver uses the ref configured in `locksmith.mainline`. The ref must resolve to a commit. With that setting unset, the driver uses the branch pointed to by `origin/HEAD` and its local counterpart. A mainline tip takes precedence over a commit on mainline history. Other ties keep `ours`, including when no mainline is available.

If a selection fails later — a peer cannot be satisfied, or a required record is missing — Locksmith does not restart from scratch. It traces the failed record back to the importer edges that selected it, flips the preference for those edges and their affected consumers, and retries with the same inputs. The retry is bounded by the number of edges involved in the failure.

With `autoDedupe` enabled, compatible transitive versions introduced by different branches can share a release when every affected parent accepts it. A locked edge or a published parent declaration provides that evidence. Without it, the instances remain separate.

### Without context

Without final declarations, `merge` performs a path-wise three-way comparison. Equal changes and one-sided changes are accepted. `packages` and `snapshots` records are atomic; snapshot dependencies merge by edge; independent peer-context changes compose into checked instances using versions already present in the inputs. A disagreement on a reachable record produces a conflict, and no merged lockfile is returned. This mode works outside Git, but it cannot decide between two valid pins.

### The Git driver

Git calls the driver with `%O %A %B %P`: the base lock, ours, theirs, and the lockfile path. The driver reads the merged workspace tree, collects manifests, configuration, and patches, and calls the contextual merge. On success it replaces `OURS` with the merged YAML.

During a rebase, the driver uses the replayed commit's original parent as the base and the current rebased `HEAD` as ours. It checks that Git's lockfiles match those revisions before using the merged workspace.

On failure, the driver writes Git's text merge of the three inputs to the lockfile and returns `1`. Git keeps the lock unmerged, so the developer can resolve the conflict or review the dependency named in the diagnostic. This matters when the text merges cleanly but the graph does not: the diagnostic still blocks the merge until someone confirms the result.

### Duplicate snapshots in raw YAML

A text merge can leave the same snapshot key twice. When the input is raw YAML, Locksmith can recover from this without preferring the first or last occurrence. It parses the duplicate values as alternatives, tries each combination, and accepts the result only when every combination produces the same validated graph, including override precedence. The search is capped at 16 combinations. Beyond that, the input produces `ResolutionRequired` instead of unbounded work.

### Merge context

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

`configurations` contains the base, ours, and theirs settings in order. For a pnpm upgrade, include the root `packageManager` string from each revision in its settings entry. Entries without it use the final root manifest's version. Among compatible graphs, Locksmith prefers one produced by the final pnpm version. `configuration` holds the merged settings. `patches` maps each configured patch path to its file contents. The Git driver collects these inputs automatically. If a required project file is conflicted, it names the file and leaves the lock unmerged.

### Worked example

The app depends on `lib@1.0.0`, which gets its peers `a` and `b` from the app. Base has `a@1.0.0` and `b@1.0.0`. Ours moves `a` to `2.0.0`. Theirs moves `a` to `3.0.0` and `b` to `2.0.0`. The merged manifest allows any version of `a` and requires `b@2.0.0`. Both branches have a valid `a`, so the target (`theirs`) breaks the tie. Only theirs has the requested `b`.

The result is one `lib@1.0.0(a@3.0.0)(b@2.0.0)` snapshot. The app's importer entry points to that snapshot, and the snapshot records `a: 3.0.0` and `b: 2.0.0`. A branch preference applies to each dependency individually; that is what allows the result to take `a` from theirs and `b` from theirs in this case, or from different branches when the constraints require it.

### Failure modes

- **Conflict**: a reachable record disagrees and no final declaration can settle it. No lockfile is returned.
- **ResolutionRequired**: a version, release declaration, or peer combination is missing. The request names what the caller should supply.
- **Unmet peer requirement**: the final peer policy rejects the result. The CLI reports the issues; `strictPeerDependencies` turns them into an error. Rust callers can use `inspect_peers` to get the same issues.

## 4. converge

`converge` starts from an installed candidate and removes the changes the edit did not require. It is the operation behind "edit one dependency, keep the rest of the lockfile".

For a cross-major pnpm upgrade, declare the exact pnpm version in each root manifest. Convergence checks the installed candidate against the final workspace and patch contents, prefers compatible baseline pins and peer bindings, and takes the bootstrap graph from the new pnpm version. Rust callers supply all final manifests, both versioned configurations, and the patch contents through `converge_with_patches` for a patched workspace.

### Inputs

- The baseline lockfile and the installed candidate.
- Before/after manifests for every changed importer.
- The baseline and final workspace configuration when catalogs, overrides, or package extensions changed.
- Optional registry metadata for missing published ranges.

### Algorithm

1. For each importer dependency, keep the baseline pin when the new manifest range still accepts it. If the range no longer accepts it, take the installed candidate's pin.
2. New dependencies take the installed candidate's pin and bring their required closure.
3. Removed dependencies drop their now-unreachable records.
4. Rebind peers per consumer. A consumer whose provider changed gets a new snapshot; unrelated consumers keep their baseline bindings. If a cycle or shared instance requires incompatible bindings, convergence keeps the installed peer assignments on those paths and can still restore unrelated child pins.
5. For workspace rule edits, identify the projects affected by each catalog, override, or package-extension change. Keep the installed edges where the new rule requires them and restore the rest. For an unchanged parent release with matching patch and peers, a locked instance is evidence for its child pins; for an upgraded parent, published dependency ranges can justify reusing an earlier child after integrity and override checks.
6. Prune unreachable records and validate the result.

### Comparison with pnpm

`pnpm install --lockfile-only` resolves the graph from the manifests. Ranges are re-evaluated, so unrelated records can move. `converge` accepts the installed graph only where the edit requires it. The result is a valid lockfile with the smallest set of changes that the edit justifies.

### Worked example

Baseline: `foo@1.0.0` depends on `leaf@1.0.0`; the app also depends on `bar@1.0.0`. The edit moves `foo` to `2.0.0`. The installed candidate has `foo@2.0.0` → `leaf@2.0.0` and `bar@1.1.0`.

`converge` keeps `foo` and `leaf` at `2.0.0` because the edit requires them. It restores `bar` to `1.0.0` because `^1.0.0` still accepts the baseline pin, and prunes `bar@1.1.0`. The app's dependency graph changes only where the edit demands it.

### Failure modes

- **Unresolved requirement**: the installed candidate lacks a record the edit requires, and no registry metadata is available.
- **Configuration mismatch**: the baseline and installed locks were generated with different resolution settings, so their pins are not comparable.
- **Unmet peer requirement**: the final peer policy rejects the converged result.

## 5. repair

`repair` fixes a lockfile that Git already merged. It starts from the current lock and changes only what the workspace evidence requires.

### Inputs

- The lockfile text.
- The manifests, workspace configuration, and patches at the same commit.
- Parent lockfiles and their configurations when a record is missing.
- Optional registry metadata.

### Algorithm

1. Parse the lock. Identical duplicate snapshot records are accepted; different contents under the same key need a decision.
2. Check configuration consistency: `settings`, `overrides`, `patchedDependencies`, `packageExtensionsChecksum`, `pnpmfileChecksum`, and `ignoredOptionalDependencies`. If the lock was generated with different rules, repair refuses; the effects extend beyond a broken record.
3. Reconcile importer declarations with the manifests: update stale version references, remove dependencies no longer declared, and add new declarations from the available evidence.
4. Compute the reachable closure from the importers. For each missing snapshot or package record, look for evidence in this order:
   - An agreeing parent lock. All parents that contain the record must agree, and their configurations must be compatible.
   - Registry metadata. A published release can supply its package record or a peer-only snapshot.
   - A contextual resolution of the current lock against the parents. This fallback uses at most two parents and handles records whose exact peer or patch identity only appears in a combination of inputs.
5. Reapply package extensions and peer bindings, then repeat until the graph is stable.
6. If the input was healthy, return it byte-for-byte.

### Comparison with pnpm

`pnpm install` rewrites the lockfile from the manifests. `repair` preserves every valid pin and only adds or removes the records needed to make the graph consistent. It also preserves the source text, which is why a healthy input comes back unchanged.

### Failure modes

- **Configuration mismatch**: the lockfile and the workspace disagree on settings, overrides, patches, or extension checksums.
- **Unresolved record**: no parent, registry entry, or contextual resolution can supply a missing record.
- **Disagreeing parents**: the available parents contain different contents for the same missing key.

## 6. Tradeoffs and boundaries

### Three commands

Each operation has a different preference model. `merge` prefers the target branch, `converge` prefers the baseline, and `repair` prefers the current lock. The inputs also differ: three locks, two locks plus manifest changes, or one lock plus workspace evidence. A single command would need a mode switch that obscures these differences.

### No I/O in the library

The library performs no filesystem, network, Git, or package-manager operations. The CLI reads project files, Git objects, and registry metadata, then passes the data in memory. This keeps the library embeddable and the operations testable without a repository.

### Offline by default

Registry access is opt-in with `--allow-network`. The lockfiles usually contain enough evidence to merge, converge, or repair without contacting a registry. When metadata is missing, the library returns a `RegistryRequest` instead of guessing, and the CLI can retry with the fetched metadata.

### Per-edge selection instead of whole-graph resolution

Locksmith selects individual records and follows their edges. This preserves unrelated pins, but it depends on evidence: a record that is not in any input and has no registry metadata cannot be invented. The alternative — resolving the whole graph — is what pnpm already does, and it is the source of the unrelated changes this tool avoids.

### Parent commits in repair

A merge commit can drop records that both parents still contain. Repair uses the commit's parents as evidence, but only when they agree and their configurations match. The direct parent search can use any number of agreeing parents; the contextual fallback uses at most two. A merge with more parents needs a more specific request.

### The 16-combination budget

Duplicate snapshot recovery is a search over interpretations. The budget bounds both work and memory. When it is exceeded, the input produces `ResolutionRequired` so the caller can choose a side or provide more evidence. An unbounded search would make a pathological input hang.

### Determinism and validation

Unchanged records are reused rather than re-rendered. Override order is preserved because it affects precedence. Before output is written, the serialized YAML is deserialized again and compared with the in-memory value; a mismatch is an error, not a partial file.

### Supported formats

Locksmith supports `lockfileVersion: '9.0'`, the default in pnpm 9–12. It reads the separate environment document that pnpm 11 and 12 write for package-manager dependencies. Experimental lockfile formats are rejected by their format version, independently of the pnpm version.

Locksmith does not install packages, does not support older lockfile formats, and does not replace pnpm's resolver. It changes the lockfile; `pnpm install` still installs from it.

### Failure contract

Standalone commands write YAML to stdout on success and diagnostics to stderr. Exit code `1` means a conflict, an unresolved dependency choice, or an unmet peer requirement; other errors use `2`. The Git driver returns `1` on failure and writes the text merge back to the lockfile, leaving Git's conflict in place. No operation writes a partial result.

### Code map

| Area | Modules | Responsibility |
| --- | --- | --- |
| Parsing and output | `source`, `verify` | YAML documents, source reuse, output comparison |
| Graph | `graph` | reachability, closure, reference validation |
| Selection | `scope`, `peer`, `reconcile` | importer edges, peer bindings, candidate selection |
| Assembly | `assembly`, `reconcile::dedupe` | snapshot and package assembly, deduplication |
| Repair | `repair` | evidence search and importer reconciliation |
