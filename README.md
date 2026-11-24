# NEAR Fetch

High level rust client library for sending transactions into the NEAR chain. This is a WIP library so use at your own risk and can likely be deprecated.

## Features

- Retrying transactions with exponential backoff.
- Multiple RPC endpoints with automatic failover, including retrying node-specific status timeouts.
- When sending transactions, the nonces are cached and invalidated locally as to avoid querying for them everytime we need to make a transaction.
- `map` and `and_then` query builders for transforming or chaining query results.
- NEP-413 off-chain message signing via `SignerExt::sign_offchain` and `OffchainMessage::verify`.
- `KeyRotatingSigner` for switching between keys on each sign point.

NOTE: this is currently not a complete replacement for near-jsonrpc-client-rs as this is more for transactions and not every single possible RPC method that you can make.
