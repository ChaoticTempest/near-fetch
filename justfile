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

# Validate near-fetch's Cargo.toml version against an expected X.Y.Z and/or require it to be strictly greater than a base X.Y.Z.
release-check expected="" base="":
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(cargo metadata --no-deps --format-version 1 \
      | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "near-fetch"))')"
    echo "near-fetch version: $version"
    if [ -n "{{expected}}" ] && [ "$version" != "{{expected}}" ]; then
      echo "error: expected {{expected}}, found $version" >&2
      exit 1
    fi
    if [ -n "{{base}}" ]; then
      semver_re='^[0-9]+\.[0-9]+\.[0-9]+$'
      if [[ ! "$version" =~ $semver_re ]]; then
        echo "error: version '$version' is not a three-component numeric version X.Y.Z" >&2
        exit 1
      fi
      if [[ ! "{{base}}" =~ $semver_re ]]; then
        echo "error: base '{{base}}' is not a three-component numeric version X.Y.Z" >&2
        exit 1
      fi
      IFS=. read -r v_major v_minor v_patch <<< "$version"
      IFS=. read -r b_major b_minor b_patch <<< "{{base}}"
      v_parts=("$v_major" "$v_minor" "$v_patch")
      b_parts=("$b_major" "$b_minor" "$b_patch")
      greater=false
      for i in 0 1 2; do
        if (( v_parts[i] > b_parts[i] )); then greater=true; break; fi
        if (( v_parts[i] < b_parts[i] )); then break; fi
      done
      if [ "$greater" != true ]; then
        echo "error: version $version is not greater than base {{base}}" >&2
        exit 1
      fi
      echo "version $version is greater than base {{base}}"
    fi

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
