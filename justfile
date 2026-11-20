set default-list

# Format the Rust workspace.
fmt:
    cargo fmt --all

# Check Rust formatting without modifying files.
fmt-check:
    cargo fmt --all --check

# Lint all Rust targets and treat warnings as errors.
lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Build the library and CLI.
build:
    cargo build --workspace --locked

# Install the locksmith CLI from this checkout.
install:
    cargo install --locked --path crates/locksmith-cli

# Run the public CLI benchmarks locally with CodSpeed.
bench mode="walltime":
    #!/bin/sh
    set -eu
    cargo build --locked --release -p locksmith-cli
    export PATH="$PWD/target/release:$PATH"
    LOCKSMITH_BENCH_DIR=$(mktemp -d)
    export LOCKSMITH_BENCH_DIR
    trap 'rm -r "$LOCKSMITH_BENCH_DIR"' EXIT
    sh benchmarks/prepare.sh "$LOCKSMITH_BENCH_DIR"
    codspeed run --mode "{{mode}}"

# Describe a user-facing change and select its release level.
change:
    knope change

# Apply pending changesets to the workspace versions and changelog.
prepare-release:
    knope prepare-release

# Run the Rust tests and documentation examples.
test:
    cargo test --workspace --all-targets --locked
    cargo test --workspace --doc --locked

# Run the same Rust checks as CI.
check: fmt-check lint test

# Run the native pnpm integration tests with pnpm on PATH or the given PNPM_BIN.
test-pnpm:
    PNPM_BIN="${PNPM_BIN:-pnpm}" cargo test --test pnpm --locked -- --ignored --nocapture --test-threads=1
