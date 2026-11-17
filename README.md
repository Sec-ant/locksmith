# locksmith

A Rust library for merging and converging pnpm lockfiles while preserving required dependency changes and reducing unrelated churn.

- `merge(base, ours, theirs)`: combine three lockfile revisions or return structured conflicts.
- `converge(baseline, candidate, changes)`: reduce changes in a pnpm-generated candidate using before-and-after `package.json` values.
- `graft(base, donor, options)`: transplant explicitly selected projects and their dependency graphs.

The library operates on in-memory lockfiles and manifests. Callers handle file access, Git operations, and dependency installation. It supports the default `lockfileVersion: '9.0'` format used by pnpm 9–12, including separate environment documents. `converge` currently requires unchanged workspace configuration.

## Usage

Requires Rust 1.89 or later. Run the three-way merge example from this repository:

```sh
cargo run --example resolve -- merge base.yaml ours.yaml theirs.yaml > merged.yaml
```

The example writes successful output to stdout and exits with `1` for conflicts or `2` for other errors. Use an output path distinct from the inputs: shell redirection truncates the destination before the command runs. The command is a library adapter example; a dedicated CLI and Git merge driver integration remain to be implemented.

Build the API documentation or run the tests:

```sh
cargo doc --workspace --no-deps --open
cargo test --workspace --locked
```

See the [design document](docs/design.md) for algorithms, input contracts, configuration boundaries, and performance tradeoffs.
