# locksmith

Locksmith merges and validates pnpm lockfiles, keeps unrelated dependency changes out of your diff, and traces peer dependency issues to the workspaces they affect.

When `pnpm-lock.yaml` conflicts during a merge or rebase, regenerating it can change unrelated versions and peer bindings. Even with the original lockfile in place, a dependency edit can change how pnpm resolves peers in other workspace projects.

Locksmith works directly on the lockfile graph and preserves versions and peer bindings that still fit. It works offline by default. Repair returns a healthy lock byte-for-byte. Merge and converge reuse unchanged source blocks and can normalize importer ordering or record formatting when assembling the result.

| Starting point                                                                      | Command                                                   |
| ----------------------------------------------------------------------------------- | --------------------------------------------------------- |
| Two branches changed the lockfile                                                   | [`merge`](#merge-two-branches)                            |
| You edited dependencies and `pnpm install` changed more than you expected           | [`converge`](#reduce-changes-after-an-install)            |
| Git already merged the lockfile, but some records are duplicated, stale, or missing | [`repair`](#repair-an-already-merged-lockfile)            |
| You want to check a lockfile for errors or compare it with the workspace             | [`validate`](#validate-a-lockfile)                        |
| You want to trace a peer warning or find packages with multiple locked instances    | [`inspect`](#inspect-peer-bindings-and-package-instances) |

## Install

Install the Node.js API and CLI as a project dependency. The package provides prebuilt binaries for Node.js 22 and later on macOS, Windows, and Linux (glibc 2.28 or later, or musl), on x64 and arm64.

```sh
pnpm add -D es-locksmith
pnpm locksmith --help
```

You can also install the CLI on macOS or Linux with Homebrew.

```sh
brew tap sec-ant/locksmith https://github.com/Sec-ant/locksmith.git
brew install sec-ant/locksmith/locksmith
```

To install from a checkout with Rust 1.89 or later, run `cargo install --locked --path crates/locksmith-cli` from the repository root.

In `package.json` scripts or with a Homebrew or Cargo install, use `locksmith` directly.

Locksmith supports `lockfileVersion: '9.0'`, the default in pnpm 9–12.

## Agent skill

The [Locksmith skill](skills/locksmith/SKILL.md) guides lockfile merges, repairs, convergence, and inspection. Install the CLI above and add `skills/locksmith/` to your agent's skills.

Try asking “Resolve the `pnpm-lock.yaml` conflict in this rebase.” The skill uses workspace context when available and enables registry lookups as needed. Say “offline” to skip them.

## Merge two branches

Pass the common ancestor, your lockfile, and the other branch's lockfile in that order. This command works outside a Git repository.

```sh
pnpm locksmith merge base.yaml ours.yaml theirs.yaml > merged.yaml
```

If both branches changed the same dependency, the final workspace manifests decide which version to keep. The Git driver collects them automatically. Scripts can supply the same information with `--context FILE`; see [Merge context](docs/design.md#merge-context). A contextual merge uses a source lockfile with compatible recorded settings. If every source differs from the final workspace, it reports the setting that needs a regenerated lockfile. Without context, Locksmith performs a three-way comparison and reports a conflict when a reachable record disagrees.

### Configure the Git merge driver

Add this line to the repository's `.gitattributes`.

```gitattributes
/pnpm-lock.yaml merge=locksmith
```

Register the driver once per clone with Git 2.38 or later. Git runs it from the worktree root. The local executable path works independently of your shell's `PATH` and skips pnpm's pre-run install checks during a merge.

```sh
git config merge.locksmith.driver './node_modules/.bin/locksmith merge --git-driver %O %A %B %P'
```

Git now calls Locksmith during merges and rebases when the lockfile needs a three-way merge. The driver reads the merged workspace declarations, selects records against them, and prefers the mainline version when both pins fit equally well.

The driver uses `origin/HEAD` by default. To use another mainline, set `locksmith.mainline` in your clone. If neither is available, ties keep `ours`.

```sh
git config --local locksmith.mainline refs/heads/trunk
```

If Git pauses on the lockfile, resolve any project files named in the diagnostic, then open `pnpm-lock.yaml`. Resolve any conflict markers. If the text merged cleanly, review the dependency named in the diagnostic. Stage the resolved files with `git add` and continue the merge or rebase.

## Reduce changes after an install

After editing dependencies or changing the pinned pnpm version, including a minor or patch upgrade, run your usual `pnpm install`. From the workspace root, pass the Git ref that holds the lockfile and workspace declarations from before your edit.

```sh
pnpm locksmith converge origin/main > converged.yaml
git diff --no-index pnpm-lock.yaml converged.yaml
```

The baseline lockfile and the installed lockfile must each match the manifests and settings they were generated from. Convergence includes generated members that match the workspace globs and appear in the baseline or installed lock. When Git has no earlier manifest, the baseline importer supplies its dependency specifiers after overrides, `dependenciesMeta`, and publish-directory settings. Locksmith keeps the changes your edit requires and restores earlier versions and peer bindings where the new declarations still accept them. See the [convergence design](docs/design.md#4-converge) for the selection rules and a worked example.

Review the diff and replace `pnpm-lock.yaml` with `converged.yaml`.

## Repair an already merged lockfile

A clean Git merge can still leave a duplicate record, a stale importer declaration, or a dependency pointing to a missing package instance. From the workspace root, check the committed lock against the manifests and settings at the same revision.

```sh
pnpm locksmith repair HEAD > repaired.yaml
git diff --no-index pnpm-lock.yaml repaired.yaml
```

Repair checks dependencies reachable from the workspace projects. It preserves unrelated, unreachable records, including stale ones. Run `locksmith validate repaired.yaml` to check every recorded reference. A healthy lock comes back byte-for-byte. Repair preserves valid pins and can recover missing records from parent commits or, with `--allow-network`, from registry metadata. When a pnpmfile is active, repair asks for effective declarations before rewriting a workspace importer. If the available evidence leaves a dependency unresolved, the command reports it on stderr and leaves stdout empty.

## Validate a lockfile

Run `locksmith validate [FILE]` to check YAML structure and internal references, including duplicate keys and missing snapshots. The path defaults to `pnpm-lock.yaml` in the current directory. This mode only reads the lockfile and runs offline.

Use `locksmith validate --workspace [FILE]` to also check that workspace members, dependency declarations, recorded settings, and configured patches match the lockfile. A `workspace:` dependency must have a local pin. A relative `workspace:` link must point to the declared path, which pnpm allows to be absent from the checkout. For a named dependency, the selected member must have the declared name and satisfy any explicit version range. Some pnpm releases reuse a frozen link after its target changes. The lockfile's directory is the workspace root. The check reads local inputs, works outside Git, and does not install packages. A workspace with `pnpm-workspace.yaml` can omit its root package manifest. The check returns `2` if a pnpmfile can change the effective declarations.

When the workspace omits `packages` and the pnpm version is unknown, Locksmith selects only the root package. Projects using the recursive default of pnpm before 10.5 can declare their pnpm version in `packageManager`.

In a merge gate, validate the final candidate lockfile. Validation checks every recorded snapshot, including those unused by the workspace. A frozen install also checks package availability and integrity.

Mixed `ignoredOptionalDependencies` rules can expose a difference between fresh and frozen installs. For example, pnpm 11.21 and later on the 11.x line keeps `is-number` on a fresh install with `["*", "!is-number"]`, then rejects that lock on a frozen install. Locksmith's writing commands preserve the fresh resolution. `validate --workspace` follows the frozen check for the workspace's pnpm version. This includes the additional pnpm 12.10+ settings when `lockfile.includeResolutionSettings` is enabled and the `lockfile.integrityTarget` setting in pnpm 12.15+.

## Inspect peer bindings and package instances

Run `pnpm locksmith inspect` from the workspace for a text report, or pass a lockfile path. Use `--json` for structured output.

```sh
pnpm locksmith inspect --json
```

For each peer finding, the JSON report includes the requester's declared range, locked bindings, matching versions already in the lock, direct and reachable workspaces, and a shortest example path. The report also lists packages with multiple releases or instances and links outside the recorded graph.

Inspection reads the lockfile and adjacent workspace settings offline. The CLI reads linked provider manifests inside the selected workspace, including linked directories that are not workspace members. In-memory callers can also supply manifests for external links. For a standalone lock, `--pnpm-version 10.33.2` selects the resolver policy explicitly. Unknown findings identify the missing evidence. Exemptions name the matching project rule. Paths follow edges recorded in the lock, including optional dependencies and each starting workspace's dev dependencies.

## Scripting contract

- On success, `merge`, `converge`, and `repair` write YAML to stdout. In Git driver mode, `merge` writes the result to Git's `OURS` input file. `inspect` writes its report to stdout and exits successfully even when it finds peer issues.
- Diagnostics go to stderr.
- `validate` is silent on success (`0`). It returns `1` for invalid lockfile content or a workspace mismatch in `--workspace` mode, and `2` when it cannot complete the check, including when the lockfile format version is unsupported.
- For other commands, exit code `1` means a conflict, an unresolved dependency choice, or an unmet peer requirement. Other errors use `2`.
- The Git driver returns `1` on failure and writes Git's text merge of the three inputs to the lockfile, so you can resolve the conflict manually. Even when the text merges cleanly, Git keeps the lock unmerged so you can review the reported dependency.

## Optional registry access

With a complete Git clone, Locksmith works offline by default. In a partial clone, Git may fetch missing objects from its remote. A contextual merge, `converge`, and `repair` can query the configured registry for package metadata with `--allow-network`. A contextual merge collects independent missing workspace and transitive dependency versions before retrying resolution. The CLI uses the project's registry settings and credentials, respects release-age and version-selection settings, and caches metadata for the command. Package installation remains pnpm's job.

## Node.js API

Use `convergeWorkspace` to compare a Git ref with an installed worktree. It returns the YAML for review and leaves the working tree untouched.

```js
import { convergeWorkspace } from 'es-locksmith'

const yaml = await convergeWorkspace('origin/main', { cwd: process.cwd() })
```

`repairWorkspace(ref, options)` reads a committed lockfile. Both workspace methods accept `cwd`, `signal`, and `allowNetwork` and use the bundled CLI. `getCliPath()` returns its executable path.

For in-memory work, `merge(base, ours, theirs, context?)`, `converge(baseline, candidate, changes, options?)`, and `repair(yaml, context)` use the Rust engine directly and return a promise for the resulting YAML. Supply workspace manifests, effective pnpm settings, patch contents and any registry evidence in the context. Pass raw YAML to a contextual merge or repair to allow duplicate-key recovery.

Use `await inspect(lock)` for YAML or a parsed lockfile, or `await inspectWorkspace({ cwd })` for a workspace on disk. Both return an `Inspection` report. The optional second argument to `inspect` supplies workspace configuration and manifests. Both `inspect` and `inspectPeers` compare the supported pnpm policies when the pnpm version is unknown.

Use `parse(yaml)` to reuse a parsed lockfile across calls. `validate`, `dependencyClosure`, and `inspectPeers` check references, trace an importer, and report peer issues. In-memory errors expose a `code` and `details`, including conflict paths and requests for missing registry metadata. Both ESM and CommonJS are supported. [TypeScript API](npm/index.d.ts)

## Rust API

Add the library with `cargo add es-locksmith` and import it as `locksmith`. The package is available on [crates.io](https://crates.io/crates/es-locksmith).

Rust applications can call `merge`, `converge`, and `repair` with in-memory inputs. Use `inspect(&lock, &rules, manifests)` for an `Inspection` report or `inspect_peers` for individual peer issues. Callers supply manifests, workspace configuration, and optional registry metadata. The library performs no I/O.

```rust
use locksmith::{Lockfile, LockfileError, merge};

fn main() -> Result<(), LockfileError> {
    let base = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}}\n")?;
    let ours = Lockfile::parse("lockfileVersion: '9.0'\nimporters: {app: {}, lib: {}}\n")?;
    let merged = merge(&base, &ours, &base, None)?;
    assert!(merged.project().unwrap()["importers"].get("lib").is_some());
    Ok(())
}
```

Run `cargo doc -p es-locksmith --no-deps --open` from a checkout for the full API. For the data model and algorithms, see [How Locksmith works](docs/design.md).

## Performance

Pull requests time `merge`, `converge`, `repair`, and `inspect` on a 314-project workspace derived from the convergence fixture. The fixture repeats one site's dependency graph 300 times and also exercises merging with the final workspace manifests and validating workspace settings. Separate loopback registry workloads resolve 64 and 512 missing dependency versions. Two further inspection workloads have 1,000 and 4,000 projects linking to an external directory. The base and PR builds take turns on the same GitHub runner. The benchmark job summary shows the median times and flags consistent slowdowns above the measured noise. [CodSpeed](https://app.codspeed.io/Sec-ant/locksmith) tracks simulated CPU work and memory for the file-based workloads. To time the commands locally, install hyperfine and run `just bench`.

## Compatibility and limits

- Supports `lockfileVersion: '9.0'`, the default in pnpm 9–12. Experimental lockfile formats are rejected by their format version.
- Locksmith never installs packages. Run `pnpm install` for that.
- The library performs no I/O. The CLI reads project files, Git, and the registry.
- Registry access is opt-in with `--allow-network`.
- The Git driver requires Git 2.38 or later.
- Lockfile commands finish assembling and checking their result before writing YAML to stdout. Use `validate` to check references throughout a lockfile, including records outside the scope of a repair.

## Prepare a release

With Knope installed, run `just change` to record a user-facing change and its version bump in `.changeset/`. Commit that file with the change. When the changeset reaches `main`, Knope Bot opens or updates a release PR with the next version, `Cargo.lock`, and `CHANGELOG.md`. See the [changeset style guide](CONTRIBUTING.md#changesets).

Review and merge the release PR to publish. GitHub Actions builds the macOS and Linux binaries, publishes the release with its artifacts, updates the Homebrew formula, and publishes `es-locksmith` to crates.io and npm.
