# NEAR Fetch

`near-fetch` is a high-level async Rust client for the NEAR protocol. It wraps
`near-jsonrpc-client` with retrying requests, automatic failover across RPC
endpoints, cached access-key nonces, and ergonomic builders for queries and
transactions, so you can read chain state or submit signed transactions without
assembling JSON-RPC requests by hand.

## Installation

```sh
cargo add near-fetch
```

## Quick start

### Create a client and run a view query

`Client::new` takes a single RPC address. Query builders such as `view`,
`view_account`, and `view_block` are awaited directly.

```rust
use near_account_id::AccountId;
use near_fetch::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("https://rpc.mainnet.near.org");
    let contract: AccountId = "social.near".parse()?;

    let result = client
        .view(&contract, "get")
        .args_json(serde_json::json!({
            "keys": ["chaotictempest.near/profile/**"]
        }))
        .await?;

    let profile: serde_json::Value = result.json()?;
    println!("{profile:#?}");
    Ok(())
}
```

### Send a transaction with retries

`send_tx` returns a retryable transaction builder. Configure a backoff strategy
with `retry_exponential`, then await it to submit the transaction.

```rust
use near_account_id::AccountId;
use near_crypto::InMemorySigner;
use near_fetch::Client;
use near_primitives::transaction::TransferAction;
use near_token::NearToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("https://rpc.testnet.near.org");
    let sender: AccountId = "alice.testnet".parse()?;
    let receiver: AccountId = "bob.testnet".parse()?;
    // TODO: load from env or secrets vault
    let secret_key = std::env::var("NEAR_SECRET_KEY")?.parse()?;
    let signer = InMemorySigner::from_secret_key(sender, secret_key);

    let outcome = client
        .send_tx(
            &signer,
            &receiver,
            vec![TransferAction {
                deposit: NearToken::from_near(1),
            }
            .into()],
        )
        .retry_exponential(100, 3) // 100 ms base delay, up to 3 retries
        .await?;

    println!("final status: {:?}", outcome.status);
    Ok(())
}
```

### Sign an off-chain message (NEP-413)

`OffchainMessage` describes a NEP-413 payload. Any `SignerExt` implementor can
sign its hash with `sign_offchain`; the same payload can be verified with
`OffchainMessage::verify`.

```rust
use near_account_id::AccountId;
use near_crypto::{InMemorySigner, SecretKey};
use near_fetch::signer::{OffchainMessage, SignerExt};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let account_id: AccountId = "alice.near".parse()?;
    // TODO: load from env or secrets vault
    let secret_key: SecretKey = std::env::var("NEAR_SECRET_KEY")?.parse()?;
    let public_key = secret_key.public_key();
    let signer = InMemorySigner::from_secret_key(account_id, secret_key);

    let message = OffchainMessage {
        message: "Hello, NEAR!".to_string(),
        nonce: [0u8; 32], // use a fresh, unique nonce in production
        recipient: "myapp.com".to_string(),
        callback_url: None,
    };
    let signature = signer.sign_offchain(&message);

    assert!(signature.verify(message.compute_hash().as_ref(), &public_key));
    Ok(())
}
```

### Fail over across RPC endpoints

Use `Client::new_with_endpoints` to configure several RPC endpoints. Requests
retry on transient failures and move to the next endpoint when one keeps
failing.

```rust
use near_fetch::Client;

fn main() {
    let client = Client::new_with_endpoints(&[
        "https://rpc.mainnet.near.org",
        "https://archival-rpc.mainnet.near.org",
    ]);
    assert_eq!(client.endpoints().len(), 2);
}
```

## Features

- **Retries with backoff** — transactions can be retried with a custom strategy
  or jittered exponential backoff via `retry` / `retry_exponential`.
- **Multi-endpoint failover** — requests fan out across configured RPC
  endpoints, retrying node-specific status timeouts and transport errors before
  moving on.
- **Nonce caching** — access-key nonces are cached locally and invalidated when
  the network reports a stale nonce, avoiding a lookup before every transaction.
- **Query builders** — `map` and `and_then` transform or chain query results
  before the query is awaited.
- **NEP-413 support** — `SignerExt::sign_offchain` signs off-chain messages and
  `OffchainMessage::verify` checks signatures and full-access keys.
- **Key rotation** — `KeyRotatingSigner` rotates through a set of keys on each
  signing operation.

## Feature flags

There are no `cfg(feature)` gates in `near-fetch` itself; every feature flag is
passed straight through to `near-jsonrpc-client`.

| Flag | Effect |
| --- | --- |
| `rustls-tls` | **Default.** Enables rustls-based TLS in `near-jsonrpc-client`. |
| `native-tls` | Alternative TLS backend. Disable default features when selecting it. |
| `any` | Enables the additional `any` RPC methods in `near-jsonrpc-client`. |
| `sandbox` | Enables the `sandbox` RPC methods in `near-jsonrpc-client`. |
| `adversarial` | Enables the `adversarial` RPC methods in `near-jsonrpc-client`. |

To use the native TLS backend instead of the default:

```sh
cargo add near-fetch --no-default-features --features native-tls
```

## Examples

Run the included examples from the repository root:

```sh
cargo run -p examples --example various_views
cargo run -p examples --example social_call
```

`various_views` queries blocks, chunks, gas price, and access keys against
testnet. `social_call` reads and writes a profile on `social.near`; replace the
placeholder secret key with your own before running it.

## Links

- [crates.io](https://crates.io/crates/near-fetch)
- [docs.rs](https://docs.rs/near-fetch)
- [Repository](https://github.com/ChaoticTempest/fetch)

Licensed under either MIT or Apache-2.0, at your option.
