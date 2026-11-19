# locksmith

Locksmith merges and repairs pnpm lockfiles and reduces unrelated changes after an install.

## Install

Build the CLI from this checkout with Rust 1.89 or later.

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

`repair` reads the lockfile, manifests, and workspace settings at the same commit. It keeps valid pins and writes the repaired lock to stdout. A healthy lock comes back byte-for-byte. If a record is missing, Locksmith can use the commit’s parents. Add `--allow-network` to also supply missing release metadata. An unresolved repair reports the dependency on stderr and produces no YAML.

## Reduce changes after an install

After you edit `package.json` or a `packageExtensions` dependency rule, `pnpm install` may also update unrelated packages. From the workspace root, run `converge` with the Git ref that contains the original manifests, workspace settings, and lockfile.

```sh
locksmith converge origin/main > converged.yaml
```

`converge` keeps baseline versions and peer bindings wherever the edit permits. For changes to `dependencies` or `optionalDependencies` in `packageExtensions`, it updates the affected package instances using the installed lock. Other workspace resolution settings must match the ref. Review `converged.yaml` before replacing `pnpm-lock.yaml`.

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

These offline benchmarks replayed one monorepo’s history on x86-64 Linux. It had 359–397 workspace projects and lockfiles of about 8–11 MiB. The release CLI ran with a warm filesystem cache. Timings include successful runs and refusals. Peak memory is resident set size (RSS) for the CLI and its child processes.

| Operation | Cases | Median time | P95 time | Median peak RSS | P95 peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| `merge --context` | 461 | 1.41 s | 4.47 s | 261 MiB | 303 MiB |
| `converge` | 68 | 1.74 s | 2.15 s | 231 MiB | 246 MiB |
| `repair` | 796 | 0.86 s | 1.07 s | 280 MiB | 331 MiB |

`merge` received a prepared workspace context. The `converge` and `repair` timings include reading from Git.

We also compared `locksmith merge --context` with pnpm 10.33.2’s `install --lockfile-only` on two generated workspaces with lockfile conflicts. Each command ran 11 times offline with a warm cache, alternating which ran first. Both reproduced a clean pnpm resolution byte-for-byte.

| Workspace projects | Lockfile size | Locksmith median time / peak RSS | pnpm median time / peak RSS |
| ---: | ---: | ---: | ---: |
| 64 | 40 KiB | 0.016 s / 8 MiB | 0.58 s / 153 MiB |
| 384 | 248 KiB | 0.086 s / 16 MiB | 1.23 s / 196 MiB |

## Rust API

Rust applications can call `merge`, `repair`, and `converge` directly. Run `cargo doc -p locksmith --no-deps --open` for the full API, or see the [architecture guide for inputs and examples](docs/design.md#rust-library-api).
