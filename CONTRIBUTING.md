# Contributing to Locksmith

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
