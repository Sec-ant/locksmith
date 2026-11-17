# locksmith design

This document is for developers integrating the Rust library or changing its implementation. It assumes familiarity with `package.json`, pnpm workspaces, and Git three-way merges. It explains how to choose an API, prepare its inputs, and interpret results or conflicts.

locksmith selects and combines dependency records from existing lockfiles. It serves two workflows: merging lockfile changes across branches, and retaining required changes while reverting unrelated version drift after a user edits manifests and runs pnpm install. pnpm handles dependency resolution and installation.

## Inputs and responsibilities

| API | Inputs | Selection policy |
| --- | --- | --- |
| `merge` | Common ancestor, ours, and theirs | Three-way changes and reachability in the resulting graph |
| `converge` | Baseline, candidate, and before-and-after manifests | Declaration changes and their workspace and peer effects |
| `graft` | Baseline, donor, and explicit options | Selected projects and top-level metadata |

All three return `Result<Lockfile, LockfileError>`. Call `to_yaml()` to serialize a successful result. Errors retain structured paths for conflicts and validation issues.

The library performs in-memory computation. Callers read Git refs, collect files, parse JSON, display diagnostics, and write results. The candidate supplied to `converge` must come from pnpm using the updated manifests. The library performs no package-manager, registry, or hook operations.

`merge` accepts three complete YAML inputs. A Git merge driver can pass the files corresponding to `%O`, `%A`, and `%B`, then write a successful result back to `%A`. The repository currently includes a file adapter example; dedicated driver integration remains to be implemented.

## Data model

`Lockfile::parse` reads `lockfileVersion: '9.0'` and identifies project and environment documents. Compatibility follows the format version; fixtures for the default pnpm 9–12 format live in `tests/fixtures`. Unknown format versions and duplicate YAML keys produce errors.

The project document contains three principal kinds of records:

- `importers`: each workspace project's direct dependency declarations and locked references.
- `packages`: package resolution information, such as integrity data and peer declarations.
- `snapshots`: dependency instances and their outgoing edges, with peer and patch context encoded in instance identifiers.

One package version can have multiple snapshots. Dependency selection preserves the complete instance identifier so that distinct peer contexts remain distinct.

Parsed inputs retain semantic data, source text, and text ranges for fields and entries. Immutable data is shared through `Arc`. Parsing and graph validation are separate operations, allowing scoped operations to select relevant donor records before validating the result.

## graft: assembling dependency graphs

`graft` transplants entire projects selected by importer path. A selected project missing from the donor represents deletion. A project missing from both inputs produces `UnknownImporter`.

Assembly considers both the selected projects' dependency closure and the closure still used by unselected projects. If both require different records under the same identifier, the operation returns a `SharedDependency` conflict. This exposes a scope conflict instead of silently changing another project's dependencies.

Top-level metadata comes from the baseline by default. `GraftOptions` can explicitly select metadata fields from the donor. Graph assembly handles structural fields.

Scoped operations remove records that become unreachable because of the change. They retain pre-existing orphan records and their dependencies to avoid unrelated cleanup.

The graft module also provides internal assembly from selected dependency edges and graph finalization. `converge` uses the former; `merge` uses the latter. The public `graft` API remains available for explicit project transplantation.

## converge: selecting changes from manifests

The typical workflow is:

1. The caller reads a lockfile and manifests from a baseline ref.
2. The user edits manifests and runs pnpm install to generate a candidate lockfile.
3. The caller supplies the baseline, candidate, and a list of `ManifestChange` values.
4. The library selects required candidate records, reuses unrelated baseline records, validates the result, and returns it.

For example, a project depends on `foo` and `bar`. The user changes only the declaration for `foo`, but installation also updates the locked version of `bar`. Convergence keeps required changes for `foo` and attempts to restore the baseline version of `bar`. Shared dependencies or peer contexts can require a broader change.

### Manifest contract

`ManifestChange` contains an importer path and `before` and `after` JSON objects. `None` on one side denotes addition or deletion. Callers must provide all manifest changes. When an affected local dependency's candidate importer changes, its before-and-after manifests are also required, even when their contents are identical: importers do not contain complete peer declarations.

The library checks the baseline against previous declarations, relevant candidate records against updated declarations, and the final result against updated declarations. Checks include npm version ranges and alias targets. Overrides recorded in the lockfile affect the effective declarations used for matching. Missing required context or a declaration mismatch produces `ManifestMismatch`.

### Selection and propagation

Convergence selects sources at the dependency-record level. Unchanged direct declarations prefer baseline resolutions; changed declarations select candidate records and their dependency closures.

Local workspace dependencies, automatically installed peers, and transitive peer contexts can expand the affected scope. The implementation follows local references and reverse dependency relationships to identify affected instances, then delegates assembly to the graft core. Changes to fields such as `name`, `version`, and `prepare` can also affect local consumers. Description changes and unrelated script changes retain the baseline lockfile.

### Workspace configuration boundary

The current `converge` contract requires unchanged workspace configuration. It reads configuration metadata already recorded in lockfiles but has no separate input for before-and-after `pnpm-workspace.yaml` configuration. Changes to manifest fields `pnpm`, `packageManager`, or `devEngines` produce `GlobalConfigurationChanged`.

Global changes to catalogs, overrides, patches, or settings require the caller to select an explicit graft importer scope and metadata policy. Manifest comparison alone does not capture all such changes. Automatic convergence of configuration changes requires an expanded configuration input contract and corresponding tests.

## merge: three-way merging

`merge` uses a common ancestor to distinguish changes made on one side, identical changes on both sides, and disagreements. Dependency records have atomic merge boundaries. For example, an importer's `specifier` and `version` are selected together, preventing a combination assembled from different sides that appeared in neither input. Catalog entries, packages, snapshots, and patch entries also have atomic boundaries.

Deletion of a graph record on one side must be evaluated against the final references: the other side may have introduced a consumer. Graph finalization removes unreachable records and conflicts while retaining conflicts that affect the result.

A conflict contains the document kind, separate path segments, conflict kind, and base/ours/theirs values. Paths use segments because package names can contain `/`. Remaining conflicts produce `Conflicts`, leaving resolution to the caller.

## Validation and text preservation

Graph validation checks constraints including importer and snapshot references, package resolution data, catalog pins, and patch hash references. Real pnpm integration tests additionally check installation compatibility.

When result data matches an input exactly, output reuses that input's text, preserving comments, a BOM, and line endings. Changed results reuse unchanged field and entry text blocks and serialize the portions that need replacement. A later pnpm write may still adjust formatting.

`to_yaml()` performs a streaming readback comparison to ensure that serialized data matches the in-memory result. The verifier compares against existing data directly, avoiding construction of a second complete document tree.

## Performance and memory

The main costs are YAML parsing, graph operations, semantic data copying, and text output. The implementation controls these costs through:

- Shared immutable input data and source text.
- Borrowed string keys, integer node identifiers, and visited flags during graph traversal, reducing allocations for long peer identifiers.
- Reuse of source text blocks and direct writes into the final output string.
- Streaming output verification to reduce peak memory during serialization.

Result construction still copies semantic data. Ordered collections, sorting, and string processing add costs beyond graph traversal, so performance should be measured with representative lockfiles. The library uses the host's allocator.

Run the staged benchmark with:

```sh
cargo bench --bench pipeline -- LOCKFILE DONOR IMPORTER
```

The benchmark reports baseline parsing, validation, graft, and rendering times. Donor parsing is outside those reported stages. Measure total process time and peak RSS separately with system profiling tools. The current benchmark entry point covers graft; dedicated merge and converge benchmarks remain to be added.

## Verification and source map

Regular tests cover public behavior, format compatibility, convergence, and property-based constraints:

```sh
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
```

Real pnpm tests require Node.js, Git, pnpm, and registry access. They cover frozen installation as well as local dependency, peer, and override scenarios:

```sh
PNPM_BIN=pnpm cargo test --test pnpm --locked -- --ignored --nocapture --test-threads=1
```

[CI](../.github/workflows/ci.yml) defines the Rust and pnpm version matrices. The main implementation files are:

| Responsibility | Files |
| --- | --- |
| Public types and entry points | [lib.rs](../crates/locksmith/src/lib.rs), [lockfile.rs](../crates/locksmith/src/lockfile.rs) |
| Convergence scope and manifest matching | [scope.rs](../crates/locksmith/src/scope.rs), [manifest.rs](../crates/locksmith/src/manifest.rs) |
| Graph assembly and traversal | [graft.rs](../crates/locksmith/src/graft.rs), [graph.rs](../crates/locksmith/src/graph.rs) |
| Three-way merging | [merge.rs](../crates/locksmith/src/merge.rs) |
| Text preservation and output verification | [source.rs](../crates/locksmith/src/source.rs), [verify.rs](../crates/locksmith/src/verify.rs) |

The organization follows [uv's resolver internals](https://docs.astral.sh/uv/reference/internals/resolver/) in explaining behavior and algorithms separately from introductory usage. uv's version-solving algorithms address a different problem. The scope, audience, and opening summary follow the guidance in [Google Technical Writing: Documents](https://developers.google.com/tech-writing/one/documents).
