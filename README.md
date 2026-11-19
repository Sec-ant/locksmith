# locksmith

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

These offline benchmarks replayed one monorepo’s history on x86-64 Linux. It had 359–398 workspace projects and lockfiles of about 8–11 MiB. The table reports Locksmith 0.1.1 with a warm filesystem cache. Each input ran once per release, alternating which version ran first on the same CPUs. Times include successful runs and refusals. P95 is the 95th percentile.

| Operation | Cases | Mean time | Median time | P95 time | Mean peak RSS | Median peak RSS | P95 peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `merge --context` | 461 | 1.88 s | 1.37 s | 4.28 s | 262 MiB | 260 MiB | 305 MiB |
| `converge` after an install | 68 | 1.30 s | 1.27 s | 1.64 s | 302 MiB | 304 MiB | 328 MiB |
| `converge` with workspace rule edits | 186 | 1.54 s | 1.63 s | 1.91 s | 300 MiB | 303 MiB | 348 MiB |
| `repair` | 796 | 0.91 s | 0.83 s | 1.94 s | 269 MiB | 281 MiB | 331 MiB |

`merge` received a prepared workspace context. The other commands read the project from Git. Peak RSS includes the CLI and its live child processes, sampled together every 10 ms and checked against the operating system’s single-process peak. During `converge`, parsing the baseline lock overlaps Git reads and parsing the installed lock. The 186-case sample covers 90 days of edits to catalogs, overrides, and package extensions.

We also compared `locksmith merge --context` with pnpm 10.33.2’s `install --lockfile-only` on two generated workspaces with lockfile conflicts. Each command ran 11 times offline with a warm cache, alternating which ran first. Both reproduced a clean pnpm resolution byte-for-byte.

| Workspace projects | Lockfile size | Locksmith median time / peak RSS | pnpm median time / peak RSS |
| ---: | ---: | ---: | ---: |
| 64 | 40 KiB | 0.015 s / 8 MiB | 0.58 s / 153 MiB |
| 384 | 248 KiB | 0.083 s / 16 MiB | 1.21 s / 196 MiB |

## Rust API

Rust applications can call `merge`, `repair`, and `converge` directly. Run `cargo doc -p locksmith --no-deps --open` for the full API, or see the [architecture guide for inputs and examples](docs/design.md#rust-library-api).
