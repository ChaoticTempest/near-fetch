# Changelog

All notable changes to `near-fetch` will be documented in this file. The format
is based on Keep a Changelog, and this project adheres to Semantic Versioning.

> **0.13.0 is a breaking release for `0.12.x` consumers.** Cargo's default caret
> requirement (`near-fetch = "0.12"`) will **not** auto-upgrade you to 0.13.0:
> you must update the requirement explicitly and follow the migration notes
> below.

## [0.13.0] - 2026-11-24

### Breaking changes

- **`ProcessQuery` trait methods renamed and now borrow `self`.**
  - Before: `into_request(self, block_ref)` / `from_response(resp)`.
  - After: `build_request(&self, block_ref)` / `process_response(&self, resp)`.
  - Migration: external implementors must rename both methods and switch the
    receiver from `self` to `&self`, cloning any owned fields they need to use.
    The built-in query builders and `ViewResult` changed to match.
- **Client accessors now follow the selected endpoint.** `Client::inner()`,
  `Client::inner_mut()` and `Client::rpc_addr()` resolve to the currently
  selected endpoint after failover rather than a fixed endpoint, and
  `From<Client> for JsonRpcClient` yields that same selected endpoint.
  - Migration: code that needs the originally configured endpoint should use
    `endpoints()[0]`.
- **`transact_async` reports the signer as the sender.**
  `RetryableTransaction::transact_async` and
  `FunctionCallTransaction::transact_async` now pass the signer's account id
  (not the transaction receiver's) to `AsyncTransactionStatus`, so
  `sender_id()` returns the signer.
- **Retry and failover behavior changed.** Node-specific handler errors
  (timeouts, unknown transactions, untracked shards, routing and internal node
  errors) are now treated as retryable and fail over to another endpoint, and
  `send_query` retries on any error because its callers are read-only and
  idempotent. Access-key lookups now use `Finality::Final` and the nonce cache
  is seeded from finalized block hashes. Expect more RPC attempts and different
  block references than before.
- **Toolchain floor raised:** edition 2024, workspace resolver 3, and
  `rust-version = "1.95"`.

### Added

- Multi-endpoint support: `Client::new_with_endpoints`, `Client::from_clients`,
  and the `endpoints`, `current_endpoint` and `current_endpoint_index`
  accessors, with per-endpoint retry and failover.
- `map` and `and_then` combinators on the query builders.
- NEP-413 off-chain message signing (#38): `OffchainMessage`,
  `SignerExt::sign_offchain` and `OffchainMessage::verify`.
- An automated `just`-based CI quality gate covering format, check, test,
  clippy, documentation and coverage.
- Release automation: tagging and GitHub Release creation on release commits.
- A version-bump check for release PRs (`just release-check`).

### Changed

- Upgraded the `near-*` dependencies to 0.38, `near-jsonrpc-client` to 0.23,
  `near-account-id` to 3 and `near-token` to 0.3.6 (#37).
- Pinned `tokio-retry` to 0.3.2 and moved RPC retries to the non-deprecated
  `Retry::start` / `RetryIf::start` API.
- Fully documented the public API and enforced it with `#![warn(missing_docs)]`.

### Fixed

- `transact_async` passed the transaction receiver as the async status sender
  id; it now passes the signer's account id.
- Transaction-status retry classification: timeouts and `UnknownTransaction`
  handler errors are retried instead of being treated as critical.
- `Function::clone` now preserves the deferred-arguments error variant
  (serialization versus I/O) instead of collapsing it.
- Corrected 9 broken intra-doc links and filled in missing public-item
  documentation.
