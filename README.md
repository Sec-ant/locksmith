# locksmith

Locksmith merges pnpm lockfiles, keeps unrelated dependency changes out of your diff, and traces peer dependency issues to the workspaces they affect.

When `pnpm-lock.yaml` conflicts during a merge or rebase, regenerating it can change unrelated versions and peer bindings. Even with the original lockfile in place, a dependency edit can change how pnpm resolves peers in other workspace projects.

Locksmith works directly on the lockfile graph and preserves versions and peer bindings that still fit. It works offline by default. Merge, converge, and repair return the original bytes when the lockfile needs no changes.

| Starting point                                                                      | Command                                                   |
| ----------------------------------------------------------------------------------- | --------------------------------------------------------- |
| Two branches changed the lockfile                                                   | [`merge`](#merge-two-branches)                            |
| You edited workspace dependencies and want to update the lockfile before installing | [`generate`](#generate-a-lockfile-from-the-workspace)          |
| You edited dependencies and `pnpm install` changed more than you expected           | [`converge`](#reduce-changes-after-an-install)            |
| Git already merged the lockfile, but some records are duplicated, stale, or missing | [`repair`](#repair-an-already-merged-lockfile)            |
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

If both branches changed the same dependency, the final workspace manifests decide which version to keep. The Git driver collects them automatically. Scripts can supply the same information with `--context FILE`; see [Merge context](docs/design.md#merge-context). Without context, Locksmith performs a three-way comparison and reports a conflict when a reachable record disagrees.

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

## Generate a lockfile from the workspace

From the workspace root, run `locksmith generate` after editing a manifest. It requires the existing `pnpm-lock.yaml`, reuses compatible pins and peer bindings, and writes the updated lockfile atomically. If another tool replaces the lockfile while generation runs, the command fails and leaves that file in place.

Generation preserves the dependencies of unchanged projects and keeps existing releases and peer bindings wherever the edit allows. It reuses releases from the lockfile when resolving new dependencies. When an edit introduces duplicate releases, it can compare alternatives on the affected paths. Use `converge` to deduplicate the rest of the graph. A workspace with `strictPeerDependencies` still requires peer compatibility.

For a release audit, `locksmith generate --exhaustive --stdout --allow-network` compares eligible assignments of registry releases in the resolver's model. It reads the manifests of alternative releases and prunes a branch when its unavoidable costs already match or exceed the best valid graph. Large version ranges and coupled peer requirements can make an exhaustive run much longer. The Node API accepts the same choice as `exhaustive: true`.

```sh
locksmith generate --allow-network
locksmith generate --stdout > preview.yaml
```

Ordinary registry dependencies need only package metadata. `--allow-network` lets Locksmith fetch missing metadata and resolve remote Git or tarball sources. Offline runs use the previous lock and the local metadata cache. Generation runs without a pnpm executable, lifecycle scripts or `node_modules`.

Set the target pnpm version with the root manifest's `packageManager` field or `--pnpm-version VERSION`. The default lock format is v9 for pnpm 9–12. The generator also reads and writes pnpm 12's explicit `lockfile.format: v13`.

Workspaces with an active pnpmfile can select a JavaScript host for `readPackage`, `updateConfig`, `preResolution` and `afterAllResolved`. The selected pnpm version determines which pre-resolution hooks run and the lockfile shape they receive. Like pnpm, generation keeps a lockfile that already matches the workspace. Once a project, catalog, override or patch changes, `readPackage` runs for every release again, because a hook can read files its checksum does not cover. Generation ranks graphs before running `afterAllResolved`, then validates the transformed lockfile.

| `--hook-runtime` | Behavior |
| --- | --- |
| `node` (default) | Starts a Node.js process in the workspace directory when it encounters a hook. The CLI uses `node` from `PATH`, or the executable in `LOCKSMITH_NODE`. The Node API uses the calling process's Node.js executable. |
| `embedded` | Uses LLRT and QuickJS compiled into the executable. Enable the optional Cargo feature with `cargo install --locked --path crates/locksmith-cli --features pnpmfile`. Use the Node host for hooks that require the full Node.js API or native addons. |
| `off` | Rejects workspaces that request hook evaluation. Workspaces configured with `ignorePnpmfile: true` generate normally. |

The default build omits the embedded runtime. A workspace without a pnpmfile needs no JavaScript host. The Node API returns the generated YAML as a string.

Generation currently requires a shared workspace lockfile and pnpm's built-in resolvers and fetchers. A `runtime:` or `tool:` dependency on Node, Bun or Deno requires a compatible entry in the previous lockfile. Settings that pnpm-workspace.yaml delegates with `include` must come from local files; a file outside the workspace root directory cannot hold settings with relative paths, and settings from config dependencies are rejected.

```js
import { generateWorkspace } from 'es-locksmith'

const yaml = await generateWorkspace({ cwd: process.cwd(), allowNetwork: true, hookRuntime: 'node' })
```

## Reduce changes after an install

After editing dependencies or upgrading pnpm, run your usual `pnpm install`. From the workspace root, pass the Git ref that holds the lockfile and workspace declarations from before your edit.

```sh
pnpm locksmith converge origin/main > converged.yaml
git diff --no-index pnpm-lock.yaml converged.yaml
```

The baseline lockfile and the installed lockfile must each match the manifests and settings they were generated from. Locksmith keeps the changes your edit requires and restores earlier versions and peer bindings where the new declarations still accept them. See the [convergence design](docs/design.md#4-converge) for the selection rules and a worked example.

Review the diff and replace `pnpm-lock.yaml` with `converged.yaml`.

## Repair an already merged lockfile

A clean Git merge can still leave a duplicate record, a stale importer declaration, or a dependency pointing to a missing package instance. From the workspace root, check the committed lock against the manifests and settings at the same revision.

```sh
pnpm locksmith repair HEAD > repaired.yaml
git diff --no-index pnpm-lock.yaml repaired.yaml
```

A healthy lock comes back byte-for-byte. Repair preserves valid pins and can recover missing records from parent commits or, with `--allow-network`, from registry metadata. When a pnpmfile is active, repair asks for effective declarations before rewriting a workspace importer. If the available evidence leaves a dependency unresolved, the command reports it on stderr and leaves stdout empty.

## Inspect peer bindings and package instances

Run `pnpm locksmith inspect` from the workspace for a text report, or pass a lockfile path. Use `--json` for structured output.

```sh
pnpm locksmith inspect --json
```

For each peer finding, the JSON report includes the requester's declared range, locked bindings, matching versions already in the lock, direct and reachable workspaces, and a shortest example path. The report also lists packages with multiple releases or instances and links outside the recorded graph.

Inspection reads the lockfile and adjacent workspace settings offline. The CLI reads linked provider manifests inside the selected workspace. In-memory callers can also supply manifests for external links. For a standalone lock, `--pnpm-version 10.33.2` selects the resolver policy explicitly. Unknown findings identify the missing evidence. Exemptions name the matching project rule. Paths follow edges recorded in the lock, including optional dependencies and each starting workspace's dev dependencies.

## Scripting contract

- On success, `merge`, `converge`, and `repair` write YAML to stdout. `inspect` writes its report to stdout and exits successfully even when it finds peer issues.
- Diagnostics go to stderr.
- Exit code `1` means a conflict, an unresolved dependency choice, or an unmet peer requirement. Other errors use `2`.
- The Git driver returns `1` on failure and writes Git's text merge of the three inputs to the lockfile, so you can resolve the conflict manually. Even when the text merges cleanly, Git keeps the lock unmerged so you can review the reported dependency.

## Optional registry access

With a complete Git clone, Locksmith works offline by default. In a partial clone, Git may fetch missing objects from its remote. A contextual merge, `converge`, and `repair` can query the configured registry for package metadata with `--allow-network`. The CLI uses the project's registry settings and credentials, respects release-age and version-selection settings, and caches metadata for the command. Package installation remains pnpm's job.

## Node.js API

Use `convergeWorkspace` to compare a Git ref with an installed worktree. It returns the YAML for review and leaves the working tree untouched.

```js
import { convergeWorkspace } from 'es-locksmith'

const yaml = await convergeWorkspace('origin/main', { cwd: process.cwd() })
```

`repairWorkspace(ref, options)` reads a committed lockfile. Both workspace methods accept `cwd`, `signal`, and `allowNetwork` and use the bundled CLI. `getCliPath()` returns its executable path.

For in-memory work, `merge(base, ours, theirs, context?)`, `converge(baseline, candidate, changes, options?)`, and `repair(yaml, context)` use the Rust engine directly and return a promise for the resulting YAML. Supply workspace manifests, effective pnpm settings, patch contents and any registry evidence in the context. Pass raw YAML to a contextual merge or repair to allow duplicate-key recovery.

Use `await inspect(lock)` for YAML or a parsed lockfile, or `await inspectWorkspace({ cwd })` for a workspace on disk. Both return an `Inspection` report. The optional second argument to `inspect` supplies workspace configuration and manifests.

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

[Notes from reading pnpm lockfiles](docs/pnpm-quirks.md) collects version-specific examples involving catalogs, local links, hooks and peers.

## Performance

Pull requests time `merge`, `converge`, `repair`, and `inspect` on a 314-project workspace derived from the convergence fixture. The fixture repeats one site's dependency graph 300 times. The base and PR builds take turns on the same GitHub runner. The benchmark job summary shows the median times and flags consistent slowdowns above the measured noise. [CodSpeed](https://app.codspeed.io/Sec-ant/locksmith) tracks simulated CPU work and memory on the same inputs. To time the commands locally, install hyperfine and run `just bench`.

## Compatibility and limits

- Supports `lockfileVersion: '9.0'`, the default in pnpm 9–12. Generation also supports pnpm 12 workspaces configured with `lockfile.format: v13`.
- Locksmith never installs packages. Run `pnpm install` for that.
- The library performs no I/O. The CLI reads project files, Git, and the registry.
- Registry access is opt-in with `--allow-network`.
- The Git driver requires Git 2.38 or later.
- The output is validated before it is written; a failed operation produces no partial lockfile.

## Prepare a release

With Knope installed, run `just change` to record a user-facing change and its version bump in `.changeset/`. Commit that file with the change. When the changeset reaches `main`, Knope Bot opens or updates a release PR with the next version, `Cargo.lock`, and `CHANGELOG.md`. See the [changeset style guide](CONTRIBUTING.md#changesets).

Review and merge the release PR to publish. GitHub Actions builds the macOS and Linux binaries, publishes the release with its artifacts, updates the Homebrew formula, and publishes `es-locksmith` to crates.io and npm.
