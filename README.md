# locksmith

**English** · [简体中文](README.zh-CN.md)

Locksmith merges and repairs pnpm lockfiles and reduces unrelated changes after an install.

## Install

Install the prebuilt CLI on macOS or Linux with Homebrew.

```sh
brew tap sec-ant/locksmith https://github.com/Sec-ant/locksmith.git
brew install sec-ant/locksmith/locksmith
```

To build from source, use Rust 1.89 or later.

```sh
cargo install --locked --path crates/locksmith-cli
```

Locksmith supports `lockfileVersion: '9.0'`, the default lockfile format in pnpm 9–12.

## Merge two branches

Pass the common ancestor first, followed by the lockfile from each branch.

```sh
locksmith merge base.yaml ours.yaml theirs.yaml > merged.yaml
```

To run Locksmith during `git merge` and `git rebase`, [configure the Git merge driver](#configure-the-git-merge-driver).

## Repair an already merged lockfile

Git can merge YAML cleanly and still leave a duplicate record or a dependency that points to a missing snapshot. Run the following commands from the workspace root.

```sh
locksmith repair HEAD > repaired.yaml
git diff --no-index pnpm-lock.yaml repaired.yaml
```

`repair` reads the lockfile, manifests, and workspace settings at the same commit. It keeps valid pins and writes the repaired lock to stdout. If an exact override applies to a required package-extension dependency, `repair` restores a missing or stale pin when the lock contains one matching instance for that consumer’s peers. A healthy lock comes back byte-for-byte. If a record is missing, Locksmith can use the commit’s parents. Add `--allow-network` to also supply missing release metadata. An unresolved repair reports the dependency on stderr and produces no YAML.

## Reduce changes after an install

After you edit a manifest, catalog, override, or package extension, `pnpm install` may also update unrelated packages. From the workspace root, run `converge` with the Git ref that contains the original manifests, workspace settings, and lockfile.

```sh
locksmith converge origin/main > converged.yaml
```

`converge` expects each input lockfile to match its own manifests and workspace settings. It keeps baseline versions that satisfy the edited declarations, applies overrides to matching dependencies, and preserves peer bindings per consumer. For example, two apps can use the same plugin with different React versions after a package extension adds React as a peer. Review the output, then use it directly as `pnpm-lock.yaml`.

If the install upgraded a package, add `--allow-network` to check whether its new release still accepts a dependency version from the baseline.

## Configure the Git merge driver

Add this rule to the repository’s `.gitattributes`.

```gitattributes
/pnpm-lock.yaml merge=locksmith
```

With Git 2.38 or later, register the driver once in each clone.

```sh
git config merge.locksmith.driver 'locksmith merge --git-driver %O %A %B %P'
```

The driver checks each dependency against the merged manifests and workspace settings. If both branches’ versions fit equally well, it prefers the mainline version. Append `--allow-network` to the driver command to look up missing release metadata in the project’s configured registry.

If `git status` shows the lockfile as unmerged, resolve any project files Locksmith names, then open the lockfile. Resolve Git’s conflict markers where edits overlap. If the text merged cleanly, review the reported dependency or configuration. Stage the resolved files with `git add`, then run `git merge --continue` or `git rebase --continue`.

## Performance

These offline benchmarks replayed the history of a monorepo with 359–398 workspace projects and 8–11 MiB lockfiles. The table reports Locksmith 0.1.2 on x86-64 Linux with a warm filesystem cache. Each input ran once per release on the same four CPUs, alternating which release ran first. The results include successful runs and refusals. P95 is the 95th percentile.

| Operation | Cases | Mean time | Median time | P95 time | Mean peak RSS | Median peak RSS | P95 peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `merge` (three files) | 461 | 1.07 s | 1.00 s | 1.98 s | 244 MiB | 244 MiB | 264 MiB |
| `merge --context` | 461 | 1.85 s | 1.36 s | 4.12 s | 262 MiB | 261 MiB | 302 MiB |
| `converge` after an install | 68 | 1.04 s | 1.00 s | 1.41 s | 312 MiB | 316 MiB | 335 MiB |
| `converge` with workspace rule edits | 186 | 1.30 s | 1.38 s | 1.63 s | 309 MiB | 310 MiB | 359 MiB |
| `repair` | 796 | 0.90 s | 0.84 s | 1.77 s | 269 MiB | 281 MiB | 331 MiB |

Both `merge` runs read three files. The `--context` run also read a prepared workspace context. The `converge` and `repair` timings include reading project data from Git. The 186 rule changes cover 90 days of edits to catalogs, overrides, and package extensions. Peak RSS includes the CLI and its live child processes, sampled together every 10 ms and checked against the operating system’s single-process peak.

We also compared `locksmith merge --context` with pnpm 10.33.2’s `install --lockfile-only` on two generated workspaces with lockfile conflicts. Each command ran 11 times offline with a warm cache, alternating which ran first. Both reproduced a clean pnpm resolution byte-for-byte.

| Workspace projects | Lockfile size | Locksmith median time / peak RSS | pnpm median time / peak RSS |
| ---: | ---: | ---: | ---: |
| 64 | 40 KiB | 0.016 s / 8 MiB | 0.59 s / 153 MiB |
| 384 | 248 KiB | 0.052 s / 17 MiB | 1.27 s / 196 MiB |

## Rust API

Rust applications can call `merge`, `repair`, and `converge` directly. Run `cargo doc -p locksmith --no-deps --open` for the full API, or see the [architecture guide for inputs and examples](docs/design.md#rust-library-api).
