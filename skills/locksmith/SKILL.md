---
name: locksmith
description: Resolves, repairs, and validates pnpm lockfiles, including checking whether a lockfile matches its workspace without installing dependencies. Also reduces unrelated lockfile changes after pnpm install and inspects peer bindings and package instances.
---

# Locksmith

Locksmith preserves pins and peer bindings allowed by the dependency declarations. It supports `lockfileVersion: '9.0'` (pnpm 9–12).

For peer warnings or excess instances in an existing lockfile, run `locksmith inspect --json` from the workspace (or pass `--pnpm-version VERSION` for a standalone lockfile). Read the declaration and binding alongside `examplePath`. A null path means no workspace reaches that binding. Check `reason` on unknown findings. Matching locked versions are candidates for review.

## Validate a lockfile

Run `locksmith validate "$lockfile"` for a read-only check of YAML and internal dependency references. Use `locksmith validate --workspace "$lockfile"` to also check workspace members, dependency declarations, and recorded settings and patches. The file defaults to `pnpm-lock.yaml`. The workspace mode reads inputs beside that file, so place a scratch candidate beside the matching workspace inputs before using it.

Both modes work offline. Exit `0` means the check passed, `1` means an invalid lockfile or workspace mismatch, and `2` means the check could not complete, for example when a pnpmfile may change the effective manifests. In a merge gate, check the lockfile from the final merge candidate. Package availability and artifact integrity are checked during installation.

## Workspace operations

Find the pnpm workspace that owns the lockfile. Check `git status --short` and the relevant manifest, configuration, and patch changes. Use the project's pnpm version. If `locksmith --version` fails because the CLI is missing, see the [installation instructions](https://github.com/Sec-ant/locksmith#install).

Use an absolute scratch directory (`scratch=$(mktemp -d)`) for inputs and output. Inspect candidates after a successful exit and clean up scratch files after verification.

Enable `--allow-network` by default where supported. Locksmith queries the project's configured registry for missing metadata as needed, using the project's credentials. Omit the flag when the task or environment requires offline operation.

## Resolve a merge or rebase

Use `git rev-parse --show-toplevel` to locate the current checkout, including in a linked worktree. For a pending merge or rebase, run `git ls-files -u -- "$lock_path"` there with the repository-relative lockfile path. Resolve conflicts in dependency declarations, settings, and patches before selecting pins.

For a worktree-only fix, copy the verified candidate to the lockfile and leave its unmerged index stages in place. Run `git add` or continue the merge or rebase only when the user explicitly asks for that action.

When stages 1, 2, and 3 exist, extract the three inputs into a scratch directory. Git's stage order applies during rebase too.

```sh
git show ":1:$lock_path" > "$scratch/base.yaml" &&
git show ":2:$lock_path" > "$scratch/ours.yaml" &&
git show ":3:$lock_path" > "$scratch/theirs.yaml"
```

Outside a pending operation, use the lockfiles from the common ancestor and the two branch tips. Prefer a contextual merge when workspace inputs are available. Build `context.json` with the [merge context reference](references/merge-context.md).

Set `workspace_root` to the absolute path of the workspace that owns the lockfile, then run:

```sh
cd "$workspace_root" &&
locksmith merge "$scratch/base.yaml" "$scratch/ours.yaml" "$scratch/theirs.yaml" \
  --context "$scratch/context.json" --allow-network > "$scratch/candidate.yaml"
```

Manual worktree resolutions require explicit context because the Git driver derives context from input revisions. With only three lockfiles, run `locksmith merge base.yaml ours.yaml theirs.yaml > "$scratch/candidate.yaml"`. If that reports a conflict, obtain the final declarations before choosing pins.

If a Git driver left the lockfile unmerged, review its diagnostic even when the text has no conflict markers. For [automatic driver setup](https://github.com/Sec-ant/locksmith#configure-the-git-merge-driver), inspect `git check-attr merge -- "$lock_path"` first and add `--allow-network` to the driver command. Linked worktrees share `git config --local` settings.

## Reduce lockfile churn after an install

From the workspace root, choose the commit that contains the lockfile and declarations from before the dependency edit. After the project's normal `pnpm install`, run:

```sh
locksmith converge --allow-network "$baseline_ref" > "$scratch/candidate.yaml"
```

Each lockfile must match its own declarations. For a pnpm upgrade (major, minor, or patch), use the pre-upgrade baseline and the lock installed with the new version. Keep the exact `packageManager` in both root manifests. If overlapping overrides select a different release, check their order under the new pnpm version. If convergence rejects a workspace setting, membership, `.npmrc`, or patch change, check the baseline and merge parent. Keep and verify the pnpm-generated lock when the rejected change is intentional.

## Repair a committed lockfile

From the workspace root, run this for stale, duplicate, or missing records after a clean text merge:

```sh
locksmith repair --allow-network "$ref" > "$scratch/candidate.yaml"
```

Repair reads the lockfile, manifests, and settings from the selected commit (`HEAD` by default), and can recover records from its parents. Verify against that same commit for a review-only repair. Use `validate --workspace` to check an uncommitted lock against its worktree inputs.

## Review and verify

- For merge, repair, and converge, exit code `1` indicates unresolved dependency evidence, a merge conflict, or a peer policy failure. Read stderr for the failing input.
- Review versions and peer bindings against the intended manifest changes. Compare with the baseline or relevant stage if the working lock still has conflict markers. For `git diff --no-index`, status `1` means the files differ.
- Run `locksmith validate` on the candidate to check internal references. To verify pnpm's manifest and settings checks, place the candidate at `pnpm-lock.yaml` in a scratch workspace with the matching manifests, settings, and patches, then run `pnpm install --frozen-lockfile --lockfile-only --ignore-scripts`. Add `--offline` for offline work. After verification, deliver or apply the candidate as requested.
