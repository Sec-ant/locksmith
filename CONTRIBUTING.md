# Contributing to Locksmith

## Commit messages

Run `just setup` once per clone to check messages during `git commit`. Linked worktrees share the hook.

Use Conventional Commits for commits and PR titles, for example `fix(merge): preserve peer bindings` or `ci: lint commit messages`. The PR title is the default subject when squash-merging. [Conventional Commits specification](https://www.conventionalcommits.org/en/v1.0.0/)

## Changesets

Run `just change` once per user-facing change. A PR that fixes two independent problems gets two files in `.changeset/`.

Write one short summary on a single line. Start with a verb and describe what changes for someone using the CLI or Rust API. Use backticks for commands, flags, and identifiers. For more examples, see [Common Changelog](https://common-changelog.org).

```markdown
---
default: patch
---

Preserve pnpm 9 and 10 publication times for direct dependencies retained by `merge --context`.
```

Knope adds the bullet. A one-line summary can keep the `#` from `just change`. When users need migration steps, use `# Short title`, leave a blank line, and put the steps below it. Knope renders that entry as a subsection.

Run `just check-changesets` to check the format and `knope --dry-run prepare-release` to preview the changelog.

## Node packages

With Node.js 22+, npm, Git and Rust installed, run `npm --prefix npm run build` to build the addon and CLI and pack them under `target/npm/`. Run `npm --prefix npm test` to install the tarballs by name from a temporary registry and test the installed package.

GNU/Linux builds also require cargo-zigbuild, Zig and GNU binutils. The pack step targets glibc 2.28 and checks the symbol requirements of both binaries. Prereleases of all npm packages use the `next` dist-tag.

The release workflow uses npm trusted publishing (OIDC). It publishes the eight platform packages before the main package. To register new names, a signed-in npm maintainer can stage a separate bootstrap version with `npm stage publish`. Keep it staged until the first release is publicly available. Rejecting the only staged version removes npm's `0.0.0-stage` placeholder and blocks reuse of the name for 24 hours. [npm staged publishing](https://docs.npmjs.com/staged-publishing/)

When the release is ready, run `npm trust github <package> --repo Sec-ant/locksmith --file release.yml --env npm --allow-publish` for each of the nine packages. The workflow must publish each package within 48 hours to validate its new trust configuration. [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/)
