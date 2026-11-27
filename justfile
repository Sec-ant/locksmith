set default-list

# Enable commit message checks for this clone and its linked worktrees.
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    tools_dir="$(cd "$(git rev-parse --git-common-dir)" && pwd)/locksmith-tools"
    export PATH="$tools_dir:$PATH"
    if ! command -v prek >/dev/null; then
        curl -fsSL https://github.com/j178/prek/releases/download/v0.5.11/prek-installer.sh | PREK_UNMANAGED_INSTALL="$tools_dir" sh
    fi
    prek install --install-hooks --allow-missing-config

# Format the Rust workspace.
fmt:
    cargo fmt --all

# Check Rust formatting without modifying files.
fmt-check:
    cargo fmt --all --check

# Lint all Rust targets and features, treating warnings as errors.
lint:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# Verify rustdoc builds without warnings.
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Build the library and CLI.
build:
    cargo build --workspace --locked

# Install the locksmith CLI from this checkout.
install:
    cargo install --locked --path crates/locksmith-cli

# Time the public CLI workloads locally with hyperfine.
bench:
    #!/bin/sh
    set -eu
    cargo build --locked --release -p locksmith-cli
    export PATH="$PWD/target/release:$PATH"
    LOCKSMITH_BENCH_DIR=$(mktemp -d)
    export LOCKSMITH_BENCH_DIR
    trap 'rm -r "$LOCKSMITH_BENCH_DIR"' EXIT
    sh benchmarks/prepare.sh "$LOCKSMITH_BENCH_DIR"
    for command in merge merge-context validate-workspace converge repair inspect inspect-external-1000 inspect-external-4000; do
        hyperfine --shell=none --warmup 2 --runs 11 "benchmarks/cli $command"
    done
    for command in registry-64 registry-512; do
        benchmarks/cli "$command" --hyperfine
    done

# Describe a user-facing change and select its release level.
change:
    knope change

# Check how pending changesets will appear in the changelog.
check-changesets:
    sh scripts/check-changesets.sh

# Apply pending changesets to the workspace versions and changelog.
prepare-release:
    knope prepare-release

# Run the Rust tests and documentation examples.
test:
    cargo test --workspace --all-targets --locked
    cargo test --workspace --exclude locksmith-node --doc --locked

# Run the same changeset and Rust checks as CI.
check: check-changesets fmt-check lint doc-check test

# Run the native pnpm integration tests with pnpm on PATH or the given PNPM_BIN.
test-pnpm:
    PNPM_BIN="${PNPM_BIN:-pnpm}" cargo test --test pnpm --locked -- --ignored --nocapture --test-threads=1
