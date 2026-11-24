# AGENTS.md

Guidance for working in this repository. Every statement below was checked
against the files and history in this checkout.

## Workspace

Root `Cargo.toml` declares a two-member workspace:

```toml
[workspace]
members = ["near-fetch", "examples"]
resolver = "3"
```

`near-fetch` is the published library; `examples` is an unpublished (`publish =
false`) package that holds runnable examples and depends on the library by path.

## Toolchain

- Both crates use `edition = "2024"` and `rust-version = "1.95"` (`near-fetch/Cargo.toml`,
  `examples/Cargo.toml`).
- There is no `rust-toolchain` / `rust-toolchain.toml` file, so the active toolchain
  is whatever is installed locally (1.96.0 in this checkout). Code must still
  compile on the declared MSRV of 1.95.

## Commands

The quality gate lives in the root `justfile`: `just ci` runs the full gate,
and `just --list` lists every available recipe. `just install` sets up the
required local tooling. The commands below make up that gate and must all pass
before handing work back:

```sh
cargo check --workspace --all-targets
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p near-fetch --no-deps
cargo llvm-cov --package near-fetch --all-targets --summary-only --fail-under-lines 95
```

`cargo doc -p near-fetch --no-deps` must emit zero warnings. CI runs three
parallel jobs on pull requests and pushes to `main`: `quality` (`just ci`),
`msrv` (Rust 1.95: `just check`, `just test`), and `features` (`just features`).

## Testing

- Tests are offline. HTTP-facing tests use `mockito = { version = "1.7",
  default-features = false }` as a dev-dependency; there are no live RPC calls.
  A test that constructs a client pointing at `127.0.0.1:1` does so to prove a
  code path short-circuits before any RPC request.
- Unit tests live in `#[cfg(test)]` modules inside `near-fetch/src/*.rs`, covering
  failover, error classification, nonce handling, query combinators, signing,
  and result decoding.
- Doctests total 5: two in `lib.rs` (`new_with_endpoints`, `send_tx`), two in
  `query.rs` (`map`, `and_then`), and one in `signer.rs` (`sign_offchain`). Four
  are marked `no_run`; the `sign_offchain` example actually executes.

## Feature flags

`near-fetch` has **no `cfg(feature)` gates in its own source**. All flags are
pass-through entries for `near-jsonrpc-client` (declared with
`default-features = false`):

- `rustls-tls` — default; maps to `near-jsonrpc-client/rustls-tls`.
- `native-tls` — alternative TLS backend; maps to `near-jsonrpc-client/native-tls`.
- `any`, `sandbox`, `adversarial` — enable the corresponding extra RPC method
  modules in `near-jsonrpc-client`.

Do not add a `cfg(feature = ...)` branch in `near-fetch` without first changing
this pattern and documenting it here.

## Architecture

- `near-fetch/src/lib.rs` — the `Client` entry point: `new`, `new_with_endpoints`,
  `from_client(s)`, query/send methods, per-endpoint retry with failover, the
  shared `current_endpoint_idx`, and the access-key nonce cache with stale-nonce
  invalidation.
- `near-fetch/src/signer.rs` — `SignerExt`, `ExposeAccountId`, `OffchainMessage`
  (NEP-413 hashing/verification), and `KeyRotatingSigner`.
- `near-fetch/src/query.rs` — `Query`/`ProcessQuery` builders for view calls,
  accounts, blocks, chunks, state, access keys, and gas price, plus the `map` and
  `and_then` combinators and `ViewResult`.
- `near-fetch/src/ops.rs` — retry builders (`RetryableTransaction`, `Transaction`,
  `FunctionCallTransaction`, `retry_exponential`), `Function` argument builders,
  and `AsyncTransactionStatus` polling.
- `near-fetch/src/result.rs` — execution results (`ExecutionFinalResult`,
  `ExecutionSuccess`, `Value`) and JSON/Borsh decoding.
- `near-fetch/src/error.rs` — the `Error` enum and `Result` alias.

## Branch and PR workflow

- Work is organized as stacked feature branches named `feat/*`, `test/*`,
  `chore/*`, and `docs/*`. The local stack is ordered
  `feat/nep413-signing` → `feat/rpc-failover` → `feat/query-combinators` →
  `feat/retry-timeouts` → `chore/edition-2024` → `test/feature-coverage` →
  `docs/rustdoc` → `docs/readme-agents`; each branch is an ancestor of the next
  and PRs are merged bottom-up. Base a new branch on the tip of the stack it
  extends.
- Version-bump branches use `release/<version>` (for example `release/0.13`)
  and version commits use `release: X` (for example `release: 0.13.0`). The
  `0.13.0` bump exists only on `release/0.13`; the crate on this stack is
  `0.12.0`.
- Commit messages follow Conventional Commits (`feat:`, `fix:`, `test:`, `docs:`,
  `chore:`, `refactor:`, `style:`).

### Release automation

When a PR titled `release: X.Y.Z` is merged to `main` and its CI run succeeds,
the `Release` workflow (`.github/workflows/release.yml`) creates a bare
annotated tag `X.Y.Z` (no `v` prefix) and a GitHub Release named after the tag
with auto-generated release notes. Nothing is published to crates.io. The
commit subject must be exactly `release: X.Y.Z`, optionally followed by
` (#N)` for squashed merges, and the version must match `near-fetch/Cargo.toml`
exactly, including all three components.

Tagging is idempotent: an existing tag is left untouched and no duplicate
Release is created. Legacy tags such as `0.11.0` and `0.12` are never rewritten
or deleted; `previous_tag` for the generated notes is simply the newest other
tag by version sort. The workflow only runs on successful `push` runs of CI on
`main` and never checks out or executes pull-request head code.

## Lockfile

`Cargo.lock` is gitignored (`.gitignore`, line 3) and must stay that way. A fresh
clone resolves dependencies from scratch, so a build can pick up newer
compatible patch versions than a previous one. Treat version-sensitive APIs
(as listed under Gotchas) as pinned by their declared requirement, not by a
committed lockfile.

## Gotchas

- **tokio-retry 0.3.2 renamed `spawn` to `start`.** `Retry::spawn` and
  `RetryIf::spawn` are deprecated; use `Retry::start(strategy, action)` and
  `RetryIf::start(strategy, action, condition)`. There is no `.retry` helper on
  tokio-retry itself. `near-fetch` has its own builder method named `retry`
  that accepts an iterator of `Duration` delays.
- **near 0.38 adaptations.** Transactions are `Transaction::V0` with
  `TransactionV0`; amounts use `near_token::NearToken`; `near-jsonrpc-client`
  is `0.23` with `default-features = false`; node timeouts appear as
  `RpcTransactionError::TimeoutError(_)`; view-state and access-key-list
  requests carry `after_key` and `limit` fields.
- **Keep the direct `borsh = "1"` dependency.** Source code imports
  `near_primitives::borsh`, but the `BorshSerialize`/`BorshDeserialize` derives
  on `OffchainMessage` generate paths that resolve through the crate's own
  `borsh` dependency. Removing it breaks the derive macros.
- **TLS feature selection.** `rustls-tls` is the default. If you build with
  `--no-default-features`, enable exactly one TLS backend explicitly, or the
  HTTP client will have no TLS implementation.

## Documentation

`lib.rs` enforces `#![warn(missing_docs)]`. Keep every public item documented and
update docs in the same change as code. Run `cargo doc -p near-fetch --no-deps`
and keep it warning-free; the five doctests are compiled by
`cargo test --workspace`.
