use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use near_jsonrpc_client::methods::tx::RpcTransactionResponse;
use tokio::sync::RwLock;

use near_account_id::AccountId;
use near_crypto::PublicKey;
use near_jsonrpc_client::errors::{JsonRpcError, JsonRpcServerError};
use near_jsonrpc_client::methods::query::RpcQueryRequest;
use near_jsonrpc_client::{methods, JsonRpcClient, MethodCallResult};
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::errors::{ActionError, ActionErrorKind, InvalidTxError, TxExecutionError};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{Action, SignedTransaction, Transaction};
use near_primitives::types::{BlockHeight, Finality, Nonce};
use near_primitives::views::{
    AccessKeyView, ExecutionStatusView, FinalExecutionOutcomeView, FinalExecutionOutcomeViewEnum,
    FinalExecutionStatus, QueryRequest, TxExecutionStatus,
};

pub mod error;
pub mod ops;
pub mod query;
pub mod result;
pub mod signer;

use crate::error::Result;
use crate::ops::RetryableTransaction;
use crate::signer::SignerExt;

pub use crate::error::Error;

/// Cache key for access key nonces.
pub type CacheKey = (AccountId, PublicKey);

/// Client that implements exponential retrying and caching of access key nonces.
#[derive(Clone, Debug)]
pub struct Client {
    rpc_client: JsonRpcClient,
    /// AccessKey nonces to reference when sending transactions.
    access_key_nonces: Arc<RwLock<HashMap<CacheKey, AtomicU64>>>,
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

    /// Internal reference to the [`JsonRpcClient`] that is utilized for all RPC calls.
    pub fn inner(&self) -> &JsonRpcClient {
        &self.rpc_client
    }

    /// Internal mutable reference to the [`JsonRpcClient`] that is utilized for all RPC calls.
    pub fn inner_mut(&mut self) -> &mut JsonRpcClient {
        &mut self.rpc_client
    }

    /// The RPC address the client is connected to.
    pub fn rpc_addr(&self) -> String {
        self.rpc_client.server_addr().into()
    }

    /// Send a series of [`Action`]s as a [`SignedTransaction`] to the network.
    /// This gives us a transaction is that retryable. To retry, simply add in a `.retry_*`
    /// method call to the end of the chain before an `.await` gets invoked.
    pub fn send_tx<'a>(
        &self,
        signer: &'a dyn SignerExt,
        receiver_id: &AccountId,
        actions: Vec<Action>,
    ) -> RetryableTransaction<'a> {
        RetryableTransaction {
            client: self.clone(),
            signer,
            actions: Ok(actions),
            receiver_id: receiver_id.clone(),
            strategy: None,
            wait_until: TxExecutionStatus::default(),
        }
    }

    pub(crate) async fn sign_tx(
        &self,
        signer: &dyn SignerExt,
        receiver_id: &AccountId,
        actions: Vec<Action>,
    ) -> Result<SignedTransaction> {
        let pk = signer.public_key();
        let (nonce, block_hash, _) = self.fetch_nonce(signer.account_id(), &pk).await?;

        let tx = Transaction::V0(near_primitives::transaction::TransactionV0 {
            nonce,
            signer_id: signer.account_id().clone(),
            public_key: pk,
            receiver_id: receiver_id.clone(),
            block_hash,
            actions,
        });

        let signature = signer.sign(tx.get_hash_and_size().0.as_ref());
        Ok(SignedTransaction::new(signature, tx))
    }

    /// Send the transaction only once. No retrying involved.
    pub(crate) async fn send_tx_once(
        &self,
        signer: &dyn SignerExt,
        receiver_id: &AccountId,
        actions: Vec<Action>,
        wait_until: TxExecutionStatus,
    ) -> Result<FinalExecutionOutcomeView> {
        let cache_key = (signer.account_id().clone(), signer.public_key());
        let signed_transaction = self.sign_tx(signer, receiver_id, actions).await?;

        let result = self
            .rpc_client
            .call(&methods::send_tx::RpcSendTransactionRequest {
                signed_transaction,
                wait_until,
            })
            .await;

        self.check_and_invalidate_cache(&cache_key, &result).await;

        let rpc_response = result.map_err(Error::from)?;
        let outcome = rpc_response.final_execution_outcome.ok_or_else(|| {
            Error::RpcReturnedInvalidData("Missing final execution outcome".to_string())
        })?;
        Ok(outcome.into_outcome())
    }

    /// Send a series of [`Action`]s as a [`SignedTransaction`] to the network. This is an async
    /// operation, where a hash is returned to reference the transaction in the future and check
    /// its status.
    pub async fn send_tx_async(
        &self,
        signer: &dyn SignerExt,
        receiver_id: &AccountId,
        actions: Vec<Action>,
    ) -> Result<CryptoHash> {
        // Note, the cache key's public-key part can be different per retry loop. For instance,
        // KeyRotatingSigner rotates secret_key and public_key after each `Signer::sign` call.
        let cache_key = (signer.account_id().clone(), signer.public_key());
        let signed_transaction = self.sign_tx(signer, receiver_id, actions).await?;
        let tx_hash = signed_transaction.get_hash();

        let result = self
            .rpc_client
            .call(&methods::send_tx::RpcSendTransactionRequest {
                signed_transaction,
                wait_until: TxExecutionStatus::None,
            })
            .await;

        if let Err(JsonRpcError::ServerError(JsonRpcServerError::HandlerError(_err))) = &result {
            // RpcBroadcastTxAsyncError should not be returned. If it does, invalidate the cache just in case.
            self.invalidate_cache(&cache_key).await;
        }

        result.map_err(Error::from)?;
        Ok(tx_hash)
    }

    /// Send a JsonRpc method to the network.
    pub(crate) async fn send_query<M>(&self, method: &M) -> MethodCallResult<M::Response, M::Error>
    where
        M: methods::RpcMethod + Send + Sync,
        M::Response: Send,
        M::Error: Send,
    {
        self.rpc_client.call(method).await
    }

    /// Fetches the nonce associated to the account id and public key, which essentially is the
    /// access key for the given account ID and public key. Utilize caching underneath to
    /// prevent querying for the same access key multiple times.
    pub async fn fetch_nonce(
        &self,
        account_id: &AccountId,
        public_key: &PublicKey,
    ) -> Result<(Nonce, CryptoHash, BlockHeight)> {
        fetch_nonce(self, account_id, public_key).await
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
            _ => Err(Error::RpcReturnedInvalidData(
                "while querying access key".into(),
            )),
        }
    }

    pub async fn check_and_invalidate_cache(
        &self,
        cache_key: &CacheKey,
        result: &Result<RpcTransactionResponse, JsonRpcError<RpcTransactionError>>,
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

    pub async fn invalidate_cache(&self, cache_key: &CacheKey) {
        let mut nonces = self.access_key_nonces.write().await;
        nonces.remove(cache_key);
    }

    /// Fetches the status of a transaction given the transaction hash.
    pub async fn status_tx_async(
        &self,
        sender_id: &AccountId,
        tx_hash: CryptoHash,
        wait_until: TxExecutionStatus,
    ) -> Result<FinalExecutionOutcomeView, Error> {
        let response = self
            .rpc_client
            .call(methods::tx::RpcTransactionStatusRequest {
                transaction_info: methods::tx::TransactionInfo::TransactionId {
                    sender_account_id: sender_id.clone(),
                    tx_hash,
                },
                wait_until,
            })
            .await
            .map_err(Error::from)?;

        if matches!(
            response.final_execution_status,
            TxExecutionStatus::None | TxExecutionStatus::Included
        ) {
            return Err(Error::RpcTransactionPending);
        }

        let outcome = response
            .final_execution_outcome
            .ok_or_else(|| {
                Error::RpcReturnedInvalidData("Missing final execution outcome".to_string())
            })?
            .into_outcome();
        Ok(outcome)
    }
}

impl From<Client> for JsonRpcClient {
    fn from(client: Client) -> Self {
        client.rpc_client
    }
}

async fn fetch_tx_errs(result: &RpcTransactionResponse) -> Vec<&TxExecutionError> {
    let mut failures = Vec::new();
    let Some(outcome) = result.final_execution_outcome.as_ref() else {
        return failures;
    };
    let outcome = match outcome {
        FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(outcome) => outcome,
        FinalExecutionOutcomeViewEnum::FinalExecutionOutcomeWithReceipt(outcome) => {
            &outcome.final_outcome
        }
    };

    if let FinalExecutionStatus::Failure(tx_err) = &outcome.status {
        failures.push(tx_err);
    }
    if let ExecutionStatusView::Failure(tx_err) = &outcome.transaction_outcome.outcome.status {
        failures.push(tx_err);
    }
    for receipt in &outcome.receipts_outcome {
        if let ExecutionStatusView::Failure(tx_err) = &receipt.outcome.status {
            failures.push(tx_err);
        }
    }
    failures
}

async fn cached_nonce(
    nonce: &AtomicU64,
    client: &Client,
) -> Result<(Nonce, CryptoHash, BlockHeight)> {
    let nonce = nonce.fetch_add(1, Ordering::SeqCst);

    // Fetch latest block_hash since the previous one is now invalid for new transactions:
    let block = client.view_block().await?;
    Ok((nonce + 1, block.header.hash, block.header.height))
}

/// Fetches the transaction nonce and block hash associated to the access key. Internally
/// caches the nonce as to not need to query for it every time, and ending up having to run
/// into contention with others.
async fn fetch_nonce(
    client: &Client,
    account_id: &AccountId,
    public_key: &PublicKey,
) -> Result<(Nonce, CryptoHash, BlockHeight)> {
    let cache_key = (account_id.clone(), public_key.clone());
    let nonces = client.access_key_nonces.read().await;
    if let Some(nonce) = nonces.get(&cache_key) {
        cached_nonce(nonce, client).await
    } else {
        drop(nonces);
        let mut nonces = client.access_key_nonces.write().await;
        match nonces.entry(cache_key) {
            // case where multiple writers end up at the same lock acquisition point and tries
            // to overwrite the cached value that a previous writer already wrote.
            Entry::Occupied(entry) => cached_nonce(entry.get(), client).await,

            // Write the cached value. This value will get invalidated when an InvalidNonce error is returned.
            Entry::Vacant(entry) => {
                let (account_id, public_key) = entry.key();
                let (access_key, block_hash, block_height) =
                    client.access_key(account_id, public_key).await?;
                entry.insert(AtomicU64::new(access_key.nonce + 1));
                Ok((access_key.nonce + 1, block_hash, block_height))
            }
        }
    }
}
