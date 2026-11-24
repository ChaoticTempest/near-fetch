//! High-level client library for transacting with and querying the NEAR network.
//!
//! [`Client`] is the main entry point: construct one with [`Client::new`] or
//! [`Client::new_with_endpoints`], then use its builder methods to send transactions
//! ([`Client::send_tx`], [`Client::batch`], [`Client::call`]) or to query chain state
//! ([`Client::view`], [`Client::view_account`], [`Client::gas_price`], and friends).
//!
//! Requests are retried on transient failures and fail over across the configured RPC
//! endpoints. Access-key nonces are cached and invalidated automatically when the network
//! reports a stale nonce.

#![warn(missing_docs)]

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use near_jsonrpc_client::methods::tx::RpcTransactionResponse;
use tokio::sync::RwLock;
use tokio_retry::{RetryIf, strategy::FixedInterval};

use near_account_id::AccountId;
use near_crypto::PublicKey;
use near_jsonrpc_client::errors::{
    JsonRpcError, JsonRpcServerError, JsonRpcServerResponseStatusError,
};
use near_jsonrpc_client::methods::query::RpcQueryRequest;
use near_jsonrpc_client::{JsonRpcClient, MethodCallResult, methods};
use near_jsonrpc_primitives::types::query::{QueryResponseKind, RpcQueryError};
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::errors::{ActionError, ActionErrorKind, InvalidTxError, TxExecutionError};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{Action, SignedTransaction, Transaction};
use near_primitives::types::{BlockHeight, Finality, Nonce};
use near_primitives::views::{
    AccessKeyView, ExecutionStatusView, FinalExecutionOutcomeView, FinalExecutionOutcomeViewEnum,
    FinalExecutionStatus, QueryRequest, TxExecutionStatus,
};

/// Error and result types returned by the client.
pub mod error;
pub mod ops;
pub mod query;
pub mod result;
/// Signing helpers, including NEP-413 off-chain message support.
pub mod signer;

use crate::error::Result;
use crate::ops::RetryableTransaction;
use crate::signer::SignerExt;

pub use crate::error::Error;

/// Cache key for access key nonces.
pub type CacheKey = (AccountId, PublicKey);

/// Total number of attempts per RPC endpoint before failing over, including the
/// initial attempt (a value of 3 means 3 attempts, i.e. 2 retries).
const RETRIES_PER_ENDPOINT: usize = 3;

/// Time to wait between each retry attempt.
const RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Client that implements retrying and caching of access key nonces, with failover
/// support across multiple RPC endpoints.
#[derive(Clone, Debug)]
pub struct Client {
    /// RPC endpoints to use, where requests fail over to the next one when
    /// the previous one keeps failing.
    rpc_endpoints: Vec<JsonRpcClient>,
    /// AccessKey nonces to reference when sending transactions.
    access_key_nonces: Arc<RwLock<HashMap<CacheKey, AtomicU64>>>,
    /// Index of the most recently selected RPC endpoint. Shared across clones so
    /// that all clones of a client fail over together.
    current_endpoint_idx: Arc<AtomicUsize>,
}

impl Client {
    /// Construct a new [`Client`] with the given RPC address.
    pub fn new(rpc_addr: &str) -> Self {
        Self::new_with_endpoints(std::slice::from_ref(&rpc_addr))
    }

    /// Construct a new [`Client`] with multiple RPC addresses, where requests will
    /// fail over to the next endpoint when the previous one fails.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use near_fetch::Client;
    ///
    /// let client = Client::new_with_endpoints(&[
    ///     "https://rpc.mainnet.near.org",
    ///     "https://archival-rpc.mainnet.near.org",
    /// ]);
    /// assert_eq!(client.endpoints().len(), 2);
    /// ```
    pub fn new_with_endpoints(rpc_addrs: &[&str]) -> Self {
        assert!(
            !rpc_addrs.is_empty(),
            "at least one RPC address is required"
        );
        let rpc_endpoints = rpc_addrs
            .iter()
            .map(|rpc_addr| JsonRpcClient::new_client().connect(*rpc_addr))
            .collect();
        Self::from_clients(rpc_endpoints)
    }

    /// Construct a [`Client`] from an existing [`JsonRpcClient`].
    pub fn from_client(client: JsonRpcClient) -> Self {
        Self::from_clients(vec![client])
    }

    /// Construct a [`Client`] from a list of existing [`JsonRpcClient`]s, where requests
    /// will failover to the next client when the previous one fails.
    pub fn from_clients(clients: Vec<JsonRpcClient>) -> Self {
        assert!(!clients.is_empty(), "at least one RPC client is required");
        Self {
            rpc_endpoints: clients,
            access_key_nonces: Arc::new(RwLock::new(HashMap::new())),
            current_endpoint_idx: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Internal reference to the most recently selected [`JsonRpcClient`] used for RPC calls.
    /// Under concurrent use, calls racing with each other may change the selection.
    pub fn inner(&self) -> &JsonRpcClient {
        &self.rpc_endpoints[self.current_endpoint_index()]
    }

    /// Internal mutable reference to the most recently selected [`JsonRpcClient`] used for
    /// RPC calls. Under concurrent use, calls racing with each other may change the selection.
    pub fn inner_mut(&mut self) -> &mut JsonRpcClient {
        let idx = self.current_endpoint_idx.load(Ordering::SeqCst);
        &mut self.rpc_endpoints[idx]
    }

    /// References to all the [`JsonRpcClient`]s, where requests fail over to the next
    /// client when the previous one fails.
    pub fn endpoints(&self) -> &[JsonRpcClient] {
        &self.rpc_endpoints
    }

    /// The most recently selected RPC endpoint. Under concurrent use, calls racing with
    /// each other may change the selection.
    pub fn current_endpoint(&self) -> &JsonRpcClient {
        &self.rpc_endpoints[self.current_endpoint_index()]
    }

    /// Index of the most recently selected RPC endpoint. Under concurrent use, calls
    /// racing with each other may change the selection.
    pub fn current_endpoint_index(&self) -> usize {
        self.current_endpoint_idx.load(Ordering::SeqCst)
    }

    /// The RPC address of the most recently selected endpoint. Under concurrent use, calls
    /// racing with each other may change the selection.
    pub fn rpc_addr(&self) -> String {
        self.current_endpoint().server_addr().into()
    }

    /// Call a JSON-RPC method with the network, retrying on retryable errors and
    /// failing over to the next RPC endpoint when one keeps failing. Errors where
    /// `is_critical` returns true abort the retrying immediately.
    pub(crate) async fn call_with_retry<M, C>(
        &self,
        method: &M,
        is_critical: C,
    ) -> MethodCallResult<M::Response, M::Error>
    where
        M: methods::RpcMethod + Send + Sync,
        M::Response: Send,
        M::Error: Send,
        C: Fn(&JsonRpcError<M::Error>) -> bool + Send + Sync,
    {
        let mut last_err = None;
        let start = self.current_endpoint_index();
        let endpoint_count = self.rpc_endpoints.len();
        for offset in 0..endpoint_count {
            let idx = (start + offset) % endpoint_count;
            let client = &self.rpc_endpoints[idx];
            // Record each failover so clones and subsequent calls start here.
            if offset > 0 {
                self.current_endpoint_idx.store(idx, Ordering::SeqCst);
            }
            let strategy = FixedInterval::new(RETRY_BACKOFF).take(RETRIES_PER_ENDPOINT - 1);
            let retry = RetryIf::start(
                strategy,
                || client.call(method),
                |err: &JsonRpcError<M::Error>| !is_critical(err),
            );
            match retry.await {
                Ok(response) => {
                    self.current_endpoint_idx.store(idx, Ordering::SeqCst);
                    return Ok(response);
                }
                Err(e) if is_critical(&e) => return Err(e),
                Err(e) => last_err = Some(e),
            }
        }
        // The index now points at the last endpoint attempted.
        Err(last_err.expect("at least one endpoint is required"))
    }

    /// Send a series of [`Action`]s as a [`SignedTransaction`] to the network.
    ///
    /// The returned [`RetryableTransaction`] can be awaited directly to send once, or
    /// configured with retries via its `retry_*` methods before awaiting.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use near_account_id::AccountId;
    /// use near_crypto::{InMemorySigner, KeyType, SecretKey};
    /// use near_fetch::Client;
    /// use near_primitives::transaction::TransferAction;
    /// use near_token::NearToken;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = Client::new("https://rpc.testnet.near.org");
    /// let sender: AccountId = "alice.testnet".parse()?;
    /// let receiver: AccountId = "bob.testnet".parse()?;
    /// let secret_key = SecretKey::from_random(KeyType::ED25519);
    /// let signer = InMemorySigner::from_secret_key(sender, secret_key);
    ///
    /// let outcome = client
    ///     .send_tx(
    ///         &signer,
    ///         &receiver,
    ///         vec![TransferAction {
    ///             deposit: NearToken::from_near(1),
    ///         }
    ///         .into()],
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
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
            .call_with_retry(
                &methods::send_tx::RpcSendTransactionRequest {
                    signed_transaction,
                    wait_until,
                },
                is_critical_tx_error,
            )
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
            .call_with_retry(
                &methods::send_tx::RpcSendTransactionRequest {
                    signed_transaction,
                    wait_until: TxExecutionStatus::None,
                },
                is_critical_tx_error,
            )
            .await;

        if let Err(JsonRpcError::ServerError(JsonRpcServerError::HandlerError(_err))) = &result {
            // RpcBroadcastTxAsyncError should not be returned. If it does, invalidate the cache just in case.
            self.invalidate_cache(&cache_key).await;
        }

        result.map_err(Error::from)?;
        Ok(tx_hash)
    }

    /// Send a JsonRpc method to the network, retrying on any error since all callers
    /// use read-only, idempotent queries.
    pub(crate) async fn send_query<M>(&self, method: &M) -> MethodCallResult<M::Response, M::Error>
    where
        M: methods::RpcMethod + Send + Sync,
        M::Response: Send,
        M::Error: Send,
    {
        self.call_with_retry(method, |_| false).await
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
            .call_with_retry(
                &RpcQueryRequest {
                    // Finality::Final is used to avoid transaction expiration when the RPC
                    // nodes are at different chain heights, e.g. on load-balanced endpoints.
                    block_reference: Finality::Final.into(),
                    request: QueryRequest::ViewAccessKey {
                        account_id: account_id.clone(),
                        public_key: public_key.clone(),
                    },
                },
                is_critical_query_error,
            )
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

    /// Invalidate the cached nonce for `cache_key` if `result` reports a stale nonce.
    ///
    /// Called after sending a transaction: an `InvalidNonce` error means the locally
    /// cached access-key nonce is behind the network and must be refetched.
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

    /// Remove the cached access-key nonce for `cache_key`, forcing a refetch on next use.
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
            .call_with_retry(
                &methods::tx::RpcTransactionStatusRequest {
                    transaction_info: methods::tx::TransactionInfo::TransactionId {
                        sender_account_id: sender_id.clone(),
                        tx_hash,
                    },
                    wait_until,
                },
                is_critical_status_error,
            )
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
        let idx = client.current_endpoint_index() % client.rpc_endpoints.len();
        client
            .rpc_endpoints
            .into_iter()
            .nth(idx)
            .expect("at least one RPC client is required")
    }
}

/// Retryable transport or server-level errors, such as a node being down or timing
/// out. Errors from the method handler itself are considered critical and are not
/// retried.
fn is_retryable_error<E>(err: &JsonRpcError<E>) -> bool {
    match err {
        JsonRpcError::TransportError(_) => true,
        JsonRpcError::ServerError(JsonRpcServerError::InternalError { .. }) => true,
        JsonRpcError::ServerError(JsonRpcServerError::ResponseStatusError(status)) => {
            matches!(
                status,
                JsonRpcServerResponseStatusError::TooManyRequests
                    | JsonRpcServerResponseStatusError::TimeoutError
                    | JsonRpcServerResponseStatusError::ServiceUnavailable
            ) || matches!(
                status,
                JsonRpcServerResponseStatusError::Unexpected { status }
                    if status.is_server_error()
            )
        }
        _ => false,
    }
}

/// Classifies errors from sending transactions. Everything invalid about the
/// transaction itself is critical, e.g. an invalid nonce, since retrying would
/// just fail again. Handler errors that merely reflect a node's view of the
/// network (timeouts, routing, shard ownership, internal node errors) are not
/// critical and should be retried on another endpoint.
fn is_critical_tx_error(err: &JsonRpcError<RpcTransactionError>) -> bool {
    if is_retryable_error(err) {
        return false;
    }
    !matches!(
        err,
        JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
            RpcTransactionError::TimeoutError(_)
                | RpcTransactionError::RequestRouted { .. }
                | RpcTransactionError::DoesNotTrackShard
                | RpcTransactionError::UnknownTransaction { .. }
                | RpcTransactionError::InternalError { .. }
        ))
    )
}

/// Classifies errors from fetching a transaction's status. Along with retryable
/// network errors, node-specific handler errors (timeouts, unknown transactions,
/// untracked shards, routing, and internal node errors) are retried, e.g. when
/// the transaction was routed to another node.
fn is_critical_status_error(err: &JsonRpcError<RpcTransactionError>) -> bool {
    if is_retryable_error(err) {
        return false;
    }
    !matches!(
        err,
        JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
            RpcTransactionError::TimeoutError(_)
                | RpcTransactionError::UnknownTransaction { .. }
                | RpcTransactionError::DoesNotTrackShard
                | RpcTransactionError::RequestRouted { .. }
                | RpcTransactionError::InternalError { .. }
        ))
    )
}

/// Classifies errors from read-only queries. Node-specific handler errors are not
/// critical: an endpoint that is still syncing or lacks the requested data can be
/// retried on another endpoint. Malformed requests remain critical.
fn is_critical_query_error(err: &JsonRpcError<RpcQueryError>) -> bool {
    if is_retryable_error(err) {
        return false;
    }
    !matches!(
        err,
        JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
            RpcQueryError::NoSyncedBlocks
                | RpcQueryError::UnavailableShard { .. }
                | RpcQueryError::GarbageCollectedBlock { .. }
                | RpcQueryError::UnknownBlock { .. }
                | RpcQueryError::InternalError { .. }
        ))
    )
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

    // Fetch the latest finalized block_hash since the previous one is now invalid
    // for new transactions, and to avoid expiration on load-balanced RPC endpoints.
    let block = client.view_block().finality(Finality::Final).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
    use near_jsonrpc_client::errors::{
        JsonRpcServerResponseStatusError, JsonRpcTransportRecvError, RpcTransportError,
    };
    use near_jsonrpc_client::methods::health::RpcHealthRequest;
    use near_jsonrpc_primitives::message::Message;
    use near_primitives::errors::InvalidTxError;

    fn jsonrpc_client(addr: &str) -> JsonRpcClient {
        JsonRpcClient::new_client().connect(addr)
    }

    #[tokio::test]
    async fn call_with_retry_fails_over_to_healthy_endpoint() {
        let mut failing = Server::new_async().await;
        let mut healthy = Server::new_async().await;

        // Every attempt against the first endpoint returns a retryable HTTP 408.
        let failing_mock = failing
            .mock("POST", "/")
            .with_status(408)
            .expect(RETRIES_PER_ENDPOINT)
            .create_async()
            .await;
        let healthy_mock = healthy
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"jsonrpc":"2.0","result":null,"id":1}"#)
            .expect(1)
            .create_async()
            .await;

        let client = Client::from_clients(vec![
            jsonrpc_client(&failing.url()),
            jsonrpc_client(&healthy.url()),
        ]);

        // Any error is retryable for this read-only query, so the failing endpoint
        // is retried RETRIES_PER_ENDPOINT times before failing over.
        let response = client.send_query(&RpcHealthRequest).await;
        assert!(response.is_ok(), "unexpected error: {response:?}");
        assert_eq!(client.current_endpoint_index(), 1);
        failing_mock.assert_async().await;
        healthy_mock.assert_async().await;
    }

    fn status_request() -> methods::tx::RpcTransactionStatusRequest {
        methods::tx::RpcTransactionStatusRequest {
            transaction_info: methods::tx::TransactionInfo::TransactionId {
                sender_account_id: "alice.near".parse().unwrap(),
                tx_hash: CryptoHash::default(),
            },
            wait_until: TxExecutionStatus::Final,
        }
    }

    fn tx_handler_error_body(err: RpcTransactionError) -> String {
        let rpc_error: near_jsonrpc_primitives::errors::RpcError = err.into();
        serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "error": rpc_error,
            "id": 1,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn status_timeout_is_retried_with_status_error_classifier() {
        let mut server = Server::new_async().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let timeout_body = tx_handler_error_body(RpcTransactionError::TimeoutError(None));
        let success_body =
            r#"{"jsonrpc":"2.0","result":{"final_execution_status":"FINAL"},"id":1}"#.to_owned();

        // First response is a node-specific handler timeout; the following one succeeds.
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |_| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    timeout_body.as_bytes().to_vec()
                } else {
                    success_body.as_bytes().to_vec()
                }
            })
            .expect_at_least(2)
            .create_async()
            .await;

        let client = Client::from_client(jsonrpc_client(&server.url()));
        let result = client
            .call_with_retry(&status_request(), is_critical_status_error)
            .await;

        assert!(result.is_ok(), "unexpected error: {result:?}");
        // If status timeouts were classified as critical, only one request would be made.
        assert!(
            attempts.load(Ordering::SeqCst) > 1,
            "status timeout was not retried"
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn critical_status_handler_error_is_not_retried() {
        let mut server = Server::new_async().await;
        let body = tx_handler_error_body(RpcTransactionError::InvalidTransaction {
            context: InvalidTxError::InvalidNonce {
                tx_nonce: 1,
                ak_nonce: 2,
            },
        });
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await;

        let client = Client::from_client(jsonrpc_client(&server.url()));
        let result = client
            .call_with_retry(&status_request(), is_critical_status_error)
            .await;

        assert!(
            result.is_err(),
            "expected a critical handler error: {result:?}"
        );
        assert!(
            matches!(
                result,
                Err(JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
                    RpcTransactionError::InvalidTransaction { .. }
                )))
            ),
            "unexpected error: {result:?}"
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn critical_error_short_circuits_without_failover() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(400)
            .expect(1)
            .create_async()
            .await;

        let client = Client::from_client(jsonrpc_client(&server.url()));
        let result = client.call_with_retry(&RpcHealthRequest, |_| true).await;

        assert!(result.is_err());
        assert_eq!(client.current_endpoint_index(), 0);
        mock.assert_async().await;
    }

    fn transport_error<E>() -> JsonRpcError<E> {
        JsonRpcError::TransportError(RpcTransportError::RecvError(
            JsonRpcTransportRecvError::UnexpectedServerResponse(Box::new(Message::notification(
                "test".into(),
                serde_json::Value::Null,
            ))),
        ))
    }

    fn server_error<E>(err: JsonRpcServerError<E>) -> JsonRpcError<E> {
        JsonRpcError::ServerError(err)
    }

    fn invalid_nonce_error() -> JsonRpcError<RpcTransactionError> {
        server_error(JsonRpcServerError::HandlerError(
            RpcTransactionError::InvalidTransaction {
                context: InvalidTxError::InvalidNonce {
                    tx_nonce: 1,
                    ak_nonce: 1,
                },
            },
        ))
    }

    #[test]
    fn transport_errors_are_retryable() {
        assert!(is_retryable_error(&transport_error::<RpcTransactionError>()));
        assert!(!is_critical_tx_error(&transport_error()));
    }

    #[test]
    fn retryable_server_errors() {
        for status in [
            JsonRpcServerResponseStatusError::TooManyRequests,
            JsonRpcServerResponseStatusError::TimeoutError,
            JsonRpcServerResponseStatusError::ServiceUnavailable,
        ] {
            let err: JsonRpcError<RpcTransactionError> =
                server_error(JsonRpcServerError::ResponseStatusError(status));
            assert!(is_retryable_error(&err));
        }

        let unexpected: JsonRpcError<RpcTransactionError> = server_error(
            JsonRpcServerError::ResponseStatusError(JsonRpcServerResponseStatusError::Unexpected {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            }),
        );
        assert!(is_retryable_error(&unexpected));

        let client_error: JsonRpcError<RpcTransactionError> = server_error(
            JsonRpcServerError::ResponseStatusError(JsonRpcServerResponseStatusError::Unexpected {
                status: reqwest::StatusCode::NOT_FOUND,
            }),
        );
        assert!(!is_retryable_error(&client_error));
    }

    #[test]
    fn invalid_transaction_errors_are_critical() {
        assert!(is_critical_tx_error(&invalid_nonce_error()));
        assert!(is_critical_status_error(&invalid_nonce_error()));
    }

    #[test]
    fn tx_handler_timeouts_are_retried() {
        let timeout = server_error(JsonRpcServerError::HandlerError(
            RpcTransactionError::TimeoutError(None),
        ));
        assert!(!is_critical_tx_error(&timeout));
        assert!(!is_critical_status_error(&timeout));

        let routed = server_error(JsonRpcServerError::HandlerError(
            RpcTransactionError::RequestRouted {
                transaction_hash: CryptoHash::default(),
            },
        ));
        assert!(!is_critical_tx_error(&routed));
    }

    #[test]
    fn status_unknown_transaction_is_retried() {
        let unknown = server_error(JsonRpcServerError::HandlerError(
            RpcTransactionError::UnknownTransaction {
                requested_transaction_hash: CryptoHash::default(),
            },
        ));
        // Node-specific errors are retried for both sends and status polling.
        assert!(!is_critical_status_error(&unknown));
        assert!(!is_critical_tx_error(&unknown));
    }

    #[test]
    fn other_handler_errors_are_critical() {
        let err = server_error(JsonRpcServerError::HandlerError(
            RpcTransactionError::DoesNotTrackShard,
        ));
        // A node that does not track the shard is retried on another endpoint.
        assert!(!is_critical_tx_error(&err));
        assert!(!is_critical_status_error(&err));
    }

    // Endpoint setup and the shared selection index are exercised through real
    // mock-server failover, not by comparing two views of the same index.

    async fn healthy_mock(server: &mut Server) -> mockito::Mock {
        server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"jsonrpc":"2.0","result":null,"id":1}"#)
            .expect(1)
            .create_async()
            .await
    }

    async fn timeout_mock(server: &mut Server) -> mockito::Mock {
        server
            .mock("POST", "/")
            .with_status(408)
            .expect(RETRIES_PER_ENDPOINT)
            .create_async()
            .await
    }

    #[tokio::test]
    async fn from_clients_fails_over_to_second_endpoint() {
        let mut first = Server::new_async().await;
        let mut second = Server::new_async().await;
        let failing = timeout_mock(&mut first).await;
        let healthy = healthy_mock(&mut second).await;

        let client = Client::from_clients(vec![
            jsonrpc_client(&first.url()),
            jsonrpc_client(&second.url()),
        ]);
        // Clone before failover: the selection index is shared across clones.
        let clone = client.clone();

        assert_eq!(client.current_endpoint_index(), 0);
        assert_eq!(client.endpoints().len(), 2);
        assert_eq!(client.rpc_addr(), first.url());
        assert!(std::ptr::eq(client.inner(), &client.endpoints()[0]));

        let response = client.send_query(&RpcHealthRequest).await;
        assert!(response.is_ok(), "unexpected error: {response:?}");

        assert_eq!(client.current_endpoint_index(), 1);
        assert!(std::ptr::eq(client.inner(), &client.endpoints()[1]));
        assert_eq!(client.rpc_addr(), second.url());
        assert_eq!(clone.current_endpoint_index(), 1);
        assert!(std::ptr::eq(clone.inner(), &clone.endpoints()[1]));
        failing.assert_async().await;
        healthy.assert_async().await;
    }

    #[tokio::test]
    async fn new_with_endpoints_fails_over_to_last_endpoint() {
        let mut first = Server::new_async().await;
        let mut second = Server::new_async().await;
        let mut third = Server::new_async().await;
        let failing_first = timeout_mock(&mut first).await;
        let failing_second = timeout_mock(&mut second).await;
        let healthy = healthy_mock(&mut third).await;

        let client = Client::new_with_endpoints(&[
            first.url().as_str(),
            second.url().as_str(),
            third.url().as_str(),
        ]);
        let clone = client.clone();

        assert_eq!(client.current_endpoint_index(), 0);
        assert_eq!(client.endpoints().len(), 3);
        assert_eq!(client.rpc_addr(), first.url());

        let response = client.send_query(&RpcHealthRequest).await;
        assert!(response.is_ok(), "unexpected error: {response:?}");

        assert_eq!(client.current_endpoint_index(), 2);
        assert!(std::ptr::eq(client.inner(), &client.endpoints()[2]));
        assert_eq!(client.rpc_addr(), third.url());
        assert_eq!(clone.current_endpoint_index(), 2);
        failing_first.assert_async().await;
        failing_second.assert_async().await;
        healthy.assert_async().await;
    }
}
