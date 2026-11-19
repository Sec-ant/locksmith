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

# Run the Rust tests and documentation examples.
test:
    cargo test --workspace --all-targets --locked
    cargo test --workspace --doc --locked

# Run the same Rust checks as CI.
check: fmt-check lint test

# Run the native pnpm integration tests with pnpm on PATH or the given PNPM_BIN.
test-pnpm:
    PNPM_BIN="${PNPM_BIN:-pnpm}" cargo test --test pnpm --locked -- --ignored --nocapture --test-threads=1
