use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;
use signer::ExposeAccountId;
use tokio::sync::RwLock;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;

use near_account_id::AccountId;
use near_crypto::{PublicKey, Signer};
use near_jsonrpc_client::errors::{JsonRpcError, JsonRpcServerError};
use near_jsonrpc_client::methods::block::RpcBlockRequest;
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
use near_jsonrpc_client::methods::query::RpcQueryRequest;
use near_jsonrpc_client::JsonRpcClient;
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::errors::{ActionError, ActionErrorKind, InvalidTxError, TxExecutionError};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{Action, SignedTransaction};
use near_primitives::types::{BlockHeight, BlockReference, Finality, Nonce};
use near_primitives::views::{
    AccessKeyView, BlockView, ExecutionStatusView, FinalExecutionOutcomeView, FinalExecutionStatus,
    QueryRequest,
};

pub mod error;
pub mod signer;

use crate::error::Result;

pub use crate::error::Error;

/// Cache key for access key nonces.
pub type CacheKey = (AccountId, PublicKey);

/// Client that implements exponential retrying and caching of access key nonces.
#[derive(Debug)]
pub struct Client {
    rpc_client: JsonRpcClient,
    /// AccessKey nonces to reference when sending transactions.
    access_key_nonces: Arc<RwLock<HashMap<CacheKey, AtomicU64>>>,
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Self {
            rpc_client: self.rpc_client.clone(),
            access_key_nonces: self.access_key_nonces.clone(),
        }
    }
}

impl Client {
    /// Construct a new [`Client`] with the given RPC address.
    pub fn new(rpc_addr: &str) -> Self {
        let connector = JsonRpcClient::new_client();
        let rpc_client = connector.connect(rpc_addr);
        Self::from_client(rpc_client)
    }

    /// Construct a [`Client`] from an existing [`JsonRpcClient`].
    pub fn from_client(client: JsonRpcClient) -> Self {
        Self {
            rpc_client: client,
            access_key_nonces: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The RPC address the client is connected to.
    pub fn rpc_addr(&self) -> String {
        self.rpc_client.server_addr().into()
    }

    /// Send a series of [`Action`]s as a [`SignedTransaction`] to the network.
    pub async fn send_tx<T: Signer + ExposeAccountId>(
        &self,
        signer: &T,
        receiver_id: &AccountId,
        actions: Vec<Action>,
    ) -> Result<FinalExecutionOutcomeView> {
        let cache_key = (signer.account_id().clone(), signer.public_key());

        retry(|| async {
            let (block_hash, nonce) = fetch_tx_nonce(self, &cache_key).await?;
            let result = self
                .rpc_client
                .call(&RpcBroadcastTxCommitRequest {
                    signed_transaction: SignedTransaction::from_actions(
                        nonce,
                        signer.account_id().clone(),
                        receiver_id.clone(),
                        signer as &dyn Signer,
                        actions.clone(),
                        block_hash,
                    ),
                })
                .await;

            self.check_and_invalidate_cache(&cache_key, &result).await;
            result.map_err(Into::into)
        })
        .await
    }

    /// View into a function.
    pub async fn view<T: Serialize, R: DeserializeOwned>(
        &self,
        receiver_id: &AccountId,
        function_name: &str,
        args: T,
    ) -> Result<R> {
        let args = match serde_json::to_vec(&args) {
            Ok(args) => args,
            Err(e) => return Err(Error::SerializeError(e)),
        };

        let resp = self
            .rpc_client
            .call(RpcQueryRequest {
                block_reference: Finality::Final.into(),
                request: QueryRequest::CallFunction {
                    account_id: receiver_id.clone(),
                    method_name: function_name.into(),
                    args: args.into(),
                },
            })
            .await?;

        let QueryResponseKind::CallResult(resp) = resp.kind else {
            return Err(Error::RpcReturnedInvalidData("while querying view"));
        };

        Ok(serde_json::from_slice(&resp.result)?)
    }

    /// Fetches the access key for the given account ID and public key.
    pub async fn access_key(
        &self,
        account_id: &AccountId,
        public_key: &PublicKey,
    ) -> Result<(AccessKeyView, CryptoHash, BlockHeight)> {
        let resp = self
            .rpc_client
            .call(&RpcQueryRequest {
                // Finality::None => Optimistic query for access key
                block_reference: Finality::None.into(),
                request: QueryRequest::ViewAccessKey {
                    account_id: account_id.clone(),
                    public_key: public_key.clone(),
                },
            })
            .await?;

        match resp.kind {
            QueryResponseKind::AccessKey(access_key) => {
                Ok((access_key, resp.block_hash, resp.block_height))
            }
            _ => Err(Error::RpcReturnedInvalidData("while querying access key")),
        }
    }

    /// Fetches the block for this block reference.
    pub async fn view_block(&self, block_reference: BlockReference) -> Result<BlockView> {
        self.rpc_client
            .call(&RpcBlockRequest { block_reference })
            .await
            .map_err(Into::into)
    }

    async fn check_and_invalidate_cache(
        &self,
        cache_key: &CacheKey,
        result: &Result<FinalExecutionOutcomeView, JsonRpcError<RpcTransactionError>>,
    ) {
        // InvalidNonce, cached nonce is potentially very far behind, so invalidate it.
        if let Err(JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
            RpcTransactionError::InvalidTransaction {
                context: InvalidTxError::InvalidNonce { .. },
                ..
            },
        ))) = result
        {
            self.invalidate_cache(cache_key).await;
        }

        let Ok(outcome) = result else {
            return;
        };
        for tx_err in fetch_tx_errs(outcome).await {
            let invalid_cache = matches!(
                tx_err,
                TxExecutionError::ActionError(ActionError {
                    kind: ActionErrorKind::DelegateActionInvalidNonce { .. },
                    ..
                }) | TxExecutionError::InvalidTxError(InvalidTxError::InvalidNonce { .. })
            );
            if invalid_cache {
                self.invalidate_cache(cache_key).await;
            }
        }
    }

    async fn invalidate_cache(&self, cache_key: &CacheKey) {
        let mut nonces = self.access_key_nonces.write().await;
        nonces.remove(cache_key);
    }
}

async fn fetch_tx_errs(result: &FinalExecutionOutcomeView) -> Vec<&TxExecutionError> {
    let mut failures = Vec::new();

    if let FinalExecutionStatus::Failure(tx_err) = &result.status {
        failures.push(tx_err);
    }
    if let ExecutionStatusView::Failure(tx_err) = &result.transaction_outcome.outcome.status {
        failures.push(tx_err);
    }
    for receipt in &result.receipts_outcome {
        if let ExecutionStatusView::Failure(tx_err) = &receipt.outcome.status {
            failures.push(tx_err);
        }
    }
    failures
}

async fn cached_nonce(nonce: &AtomicU64, client: &Client) -> Result<(CryptoHash, Nonce)> {
    let nonce = nonce.fetch_add(1, Ordering::SeqCst);

    // Fetch latest block_hash since the previous one is now invalid for new transactions:
    let block = client.view_block(Finality::Final.into()).await?;
    let block_hash = block.header.hash;
    Ok((block_hash, nonce + 1))
}

/// Fetches the transaction nonce and block hash associated to the access key. Internally
/// caches the nonce as to not need to query for it every time, and ending up having to run
/// into contention with others.
async fn fetch_tx_nonce(client: &Client, cache_key: &CacheKey) -> Result<(CryptoHash, Nonce)> {
    let nonces = client.access_key_nonces.read().await;
    if let Some(nonce) = nonces.get(cache_key) {
        cached_nonce(nonce, client).await
    } else {
        drop(nonces);
        let mut nonces = client.access_key_nonces.write().await;
        match nonces.entry(cache_key.clone()) {
            // case where multiple writers end up at the same lock acquisition point and tries
            // to overwrite the cached value that a previous writer already wrote.
            Entry::Occupied(entry) => cached_nonce(entry.get(), client).await,

            // Write the cached value. This value will get invalidated when an InvalidNonce error is returned.
            Entry::Vacant(entry) => {
                let (account_id, public_key) = entry.key();
                let (access_key, block_hash, _) = client.access_key(account_id, public_key).await?;
                entry.insert(AtomicU64::new(access_key.nonce + 1));
                Ok((block_hash, access_key.nonce + 1))
            }
        }
    }
}

async fn retry<R, E, T, F>(task: F) -> T::Output
where
    F: FnMut() -> T,
    T: core::future::Future<Output = core::result::Result<R, E>>,
{
    // Exponential backoff starting w/ 5ms for maximum retry of 4 times with the following delays:
    //   5, 25, 125, 625 ms
    let retry_strategy = ExponentialBackoff::from_millis(5).map(jitter).take(4);
    Retry::spawn(retry_strategy, task).await
}
