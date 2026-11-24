# Reproducible quality gates for near-fetch. Run `just --list` to see recipes.

cov_floor := "95"

# List available recipes (default).
default:
    just --list

# Install the toolchain components and tooling used by `just ci`.
install:
    rustup component add rustfmt clippy llvm-tools-preview
    cargo install cargo-llvm-cov --locked

# Format the workspace.
fmt:
    cargo fmt --all

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all -- --check

# Type-check the whole workspace, including tests and examples.
check:
    cargo check --workspace --all-targets

# Run unit tests and doctests.
test:
    cargo test --workspace

# Lint the workspace with warnings denied.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Build documentation with warnings denied.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc -p near-fetch --no-deps

# Report line/region coverage for the library.
coverage:
    cargo llvm-cov --package near-fetch --all-targets --summary-only

# Enforce the minimum line coverage floor.
coverage-check:
    cargo llvm-cov --package near-fetch --all-targets --summary-only --fail-under-lines {{cov_floor}}

# Check all feature combinations, including each TLS backend individually.
features:
    cargo check --workspace --all-targets --all-features
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    cargo check --workspace --all-targets --no-default-features
    cargo check --workspace --all-targets --no-default-features --features rustls-tls
    cargo check --workspace --all-targets --no-default-features --features native-tls
    cargo check --workspace --all-targets --no-default-features --features adversarial
    cargo check --workspace --all-targets --no-default-features --features any
    cargo check --workspace --all-targets --no-default-features --features sandbox

# Full aggregate quality gate.
ci: fmt-check check test clippy doc coverage-check
