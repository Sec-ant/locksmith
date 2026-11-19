# locksmith

**English** · [简体中文](README.zh-CN.md)

Locksmith merges pnpm lockfiles and keeps unrelated dependency changes out of your diff.

| Starting point | Command |
| --- | --- |
| Two branches changed the lockfile | [`merge`](#merge-two-branches) |
| You edited dependencies and `pnpm install` changed more than you expected | [`converge`](#reduce-changes-after-an-install) |
| Git already merged the lockfile, but some records are duplicated, stale, or missing | [`repair`](#repair-an-already-merged-lockfile) |

## Install

Install the prebuilt CLI on macOS or Linux with Homebrew.

```sh
brew tap sec-ant/locksmith https://github.com/Sec-ant/locksmith.git
brew install sec-ant/locksmith/locksmith
```

To install from a checkout with Rust 1.89 or later, run `cargo install --locked --path crates/locksmith-cli` from the repository root.

Locksmith supports `lockfileVersion: '9.0'`, the default in pnpm 9–12.

## Merge two branches

Pass the common ancestor, your lockfile, and the other branch’s lockfile in that order. This command works outside a Git repository.

```sh
locksmith merge base.yaml ours.yaml theirs.yaml > merged.yaml
```

If both branches changed the same dependency, the final workspace manifests can settle which version to use. The Git driver collects them automatically. Scripts can supply the same information with `--context FILE`. [Merge context format](docs/design.md#merge-context)

### Configure the Git merge driver

Add this line to the repository’s `.gitattributes`.

```gitattributes
/pnpm-lock.yaml merge=locksmith
```

Register the driver once per clone with Git 2.38 or later.

```sh
git config merge.locksmith.driver 'locksmith merge --git-driver %O %A %B %P'
```

Git now calls Locksmith during merges and rebases when the lockfile needs a three-way merge. The driver checks each dependency against the merged workspace and prefers the mainline version when both pins fit equally well.

If Git pauses on the lockfile, resolve any project files named in the diagnostic, then open `pnpm-lock.yaml`. Resolve any conflict markers. If the text merged cleanly, review the dependency named in the diagnostic. Stage the resolved files with `git add` and continue the merge or rebase.

## Reduce changes after an install

After editing a manifest, catalog, override, or package extension, run your usual `pnpm install`. From the workspace root, pass the Git ref that holds the lockfile and workspace declarations from before your edit.

```sh
locksmith converge origin/main > converged.yaml
git diff --no-index pnpm-lock.yaml converged.yaml
```

Each lockfile should match its own workspace declarations. Locksmith keeps the changes your edit requires and restores earlier versions and peer bindings where they still fit. After a successful run, review the diff and replace `pnpm-lock.yaml` with `converged.yaml`.

## Repair an already merged lockfile

A clean Git merge can still leave a duplicate record or a dependency pointing to a missing package instance. From the workspace root, check the committed lock against the manifests and settings at the same revision.

```sh
locksmith repair HEAD > repaired.yaml
git diff --no-index pnpm-lock.yaml repaired.yaml
```

A healthy lock comes back byte-for-byte. Repair preserves valid pins and can recover missing records from parent commits. On success, review and apply any changes. If the available evidence leaves a dependency unresolved, the command reports it on stderr and leaves stdout empty.

## Optional registry access

Locksmith works offline by default. A merge with workspace context, `converge`, or `repair` can query the configured registry for package metadata with `--allow-network`. Use pnpm to install packages.

## Rust API

Rust applications can call `merge`, `converge`, and `repair` with in-memory inputs. From a checkout, run `cargo doc -p locksmith --no-deps --open` for the API reference. [Architecture and library examples](docs/design.md#rust-library-api)

## Performance

On a monorepo with 359–398 workspace projects and 8–11 MiB lockfiles, median command times ranged from **0.84 s to 1.38 s**, with median peak memory of **244–316 MiB**. A three-file merge took 1.00 s at the median and 1.98 s at P95.

<details>
<summary>Full measurements and comparison with pnpm</summary>

These offline results cover Locksmith 0.1.2 on x86-64 Linux with a warm filesystem cache. Each input ran once per release on the same four CPUs, alternating which release ran first. Times include successes and refusals. P95 is the 95th percentile.

| Operation | Cases | Mean time | Median time | P95 time | Mean peak RSS | Median peak RSS | P95 peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `merge` (three files) | 461 | 1.07 s | 1.00 s | 1.98 s | 244 MiB | 244 MiB | 264 MiB |
| `merge --context` | 461 | 1.85 s | 1.36 s | 4.12 s | 262 MiB | 261 MiB | 302 MiB |
| `converge` after an install | 68 | 1.04 s | 1.00 s | 1.41 s | 312 MiB | 316 MiB | 335 MiB |
| `converge` with workspace rule edits | 186 | 1.30 s | 1.38 s | 1.63 s | 309 MiB | 310 MiB | 359 MiB |
| `repair` | 796 | 0.90 s | 0.84 s | 1.77 s | 269 MiB | 281 MiB | 331 MiB |

Both merge runs read three lockfiles. The context run also read prepared workspace data. The other commands include time spent reading from Git. The 186 rule edits cover 90 days of changes to catalogs, overrides, and package extensions. Peak RSS (resident memory) is the larger of the process-tree total sampled every 10 ms and the operating system’s single-process peak.

In a separate comparison, we ran `locksmith merge --context` and pnpm 10.33.2’s `install --lockfile-only` on two generated workspaces with lockfile conflicts. Each command ran 11 times offline with a warm cache, alternating which ran first. Both outputs matched a clean pnpm resolution byte-for-byte.

| Workspace projects | Lockfile size | Locksmith median time / peak RSS | pnpm median time / peak RSS |
| ---: | ---: | ---: | ---: |
| 64 | 40 KiB | 0.016 s / 8 MiB | 0.59 s / 153 MiB |
| 384 | 248 KiB | 0.052 s / 17 MiB | 1.27 s / 196 MiB |

</details>
