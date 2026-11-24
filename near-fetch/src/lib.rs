//! High-level client library for transacting with and querying the NEAR network.
//!
//! [`Client`] is the main entry point: construct one with [`Client::new`] or
//! [`Client::builder`], then use its builder methods to send transactions
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

#[cfg(test)]
mod testkit;

use crate::error::Result;
use crate::ops::RetryableTransaction;
use crate::signer::SignerExt;

pub use crate::error::Error;

/// Cache key for access key nonces.
pub type CacheKey = (AccountId, PublicKey);

/// Default delay between retry attempts used by [`ClientBuilder`].
const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Default number of retries after the initial attempt on each endpoint.
const DEFAULT_RETRIES_PER_ENDPOINT: usize = 2;

/// Default timeout applied to each individual RPC attempt.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default delay between async transaction status polls.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(300);

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
    /// Delay between retry attempts on each endpoint.
    retry_backoff: Duration,
    /// Number of retries after the initial attempt on each endpoint.
    retries_per_endpoint: usize,
    /// Timeout applied to each individual RPC attempt.
    request_timeout: Duration,
    /// Delay between async transaction status polls.
    poll_interval: Duration,
    /// Overall deadline for async transaction status polling, if any.
    poll_timeout: Option<Duration>,
}

/// Builder for a [`Client`]: configures RPC endpoints and per-endpoint retry behavior.
///
/// # Example
///
/// ```rust,no_run
/// use near_fetch::Client;
///
/// let client = Client::builder()
///     .endpoints(&[
///         "https://rpc.mainnet.near.org",
///         "https://archival-rpc.mainnet.near.org",
///     ])
///     .build();
/// assert_eq!(client.endpoints().len(), 2);
/// ```
#[derive(Clone, Debug)]
pub struct ClientBuilder {
    rpc_endpoints: Vec<JsonRpcClient>,
    retry_backoff: Duration,
    retries_per_endpoint: usize,
    request_timeout: Duration,
    poll_interval: Duration,
    poll_timeout: Option<Duration>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            rpc_endpoints: Vec::new(),
            retry_backoff: DEFAULT_RETRY_BACKOFF,
            retries_per_endpoint: DEFAULT_RETRIES_PER_ENDPOINT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            poll_timeout: None,
        }
    }
}

impl ClientBuilder {
    /// Append a JSON-RPC endpoint created from `rpc_addr` to the endpoint list.
    pub fn endpoint(mut self, rpc_addr: &str) -> Self {
        self.rpc_endpoints
            .push(JsonRpcClient::new_client().connect(rpc_addr));
        self
    }

    /// Append each address in `rpc_addrs` as an endpoint, in order.
    pub fn endpoints(self, rpc_addrs: &[&str]) -> Self {
        rpc_addrs
            .iter()
            .fold(self, |builder, rpc_addr| builder.endpoint(rpc_addr))
    }

    /// Append existing [`JsonRpcClient`]s to the endpoint list, in iteration order.
    pub fn clients(mut self, clients: impl IntoIterator<Item = JsonRpcClient>) -> Self {
        self.rpc_endpoints.extend(clients);
        self
    }

    /// Set the delay between retry attempts on each endpoint.
    pub fn retry_backoff(mut self, retry_backoff: Duration) -> Self {
        self.retry_backoff = retry_backoff;
        self
    }

    /// Set the number of retries *after* the initial attempt on each endpoint.
    ///
    /// A value of `n` allows up to `n + 1` attempts per endpoint before failing
    /// over to the next one. The default is 2, i.e. 3 attempts per endpoint.
    pub fn retries_per_endpoint(mut self, retries_per_endpoint: usize) -> Self {
        self.retries_per_endpoint = retries_per_endpoint;
        self
    }

    /// Set the timeout applied to *each* RPC attempt.
    ///
    /// Defaults to 30 seconds. A hung endpoint now times out instead of blocking
    /// forever: the timeout is treated as a retryable error, so the attempt is
    /// retried and then failed over to the next endpoint.
    pub fn request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Set the delay between async transaction status polls.
    ///
    /// Defaults to 300 ms. Used by [`crate::ops::AsyncTransactionStatus::wait`]
    /// via `wait_default` and `IntoFuture`, replacing the previously hard-coded
    /// 300 ms interval.
    pub fn poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Set an overall deadline for async transaction status polling.
    ///
    /// Defaults to `None`, meaning polling continues until the transaction
    /// completes (the previous behavior). When set, exceeding the deadline
    /// returns [`crate::Error::PollTimeout`] instead of polling forever.
    pub fn poll_timeout(mut self, poll_timeout: Option<Duration>) -> Self {
        self.poll_timeout = poll_timeout;
        self
    }

    /// Build a [`Client`] from the configured endpoints and retry settings.
    ///
    /// # Panics
    ///
    /// Panics if no endpoints were added via [`Self::endpoint`], [`Self::endpoints`]
    /// or [`Self::clients`].
    pub fn build(self) -> Client {
        assert!(
            !self.rpc_endpoints.is_empty(),
            "at least one RPC endpoint is required"
        );
        Client {
            rpc_endpoints: self.rpc_endpoints,
            access_key_nonces: Arc::new(RwLock::new(HashMap::new())),
            current_endpoint_idx: Arc::new(AtomicUsize::new(0)),
            retry_backoff: self.retry_backoff,
            retries_per_endpoint: self.retries_per_endpoint,
            request_timeout: self.request_timeout,
            poll_interval: self.poll_interval,
            poll_timeout: self.poll_timeout,
        }
    }
}

impl Client {
    /// Construct a new [`Client`] with the given RPC address.
    ///
    /// Equivalent to `Client::builder().endpoint(rpc_addr).build()`.
    pub fn new(rpc_addr: &str) -> Self {
        Self::builder().endpoint(rpc_addr).build()
    }

    /// Start configuring a [`Client`] with endpoints and retry settings.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Construct a [`Client`] from an existing [`JsonRpcClient`].
    ///
    /// Equivalent to `Client::builder().clients([client]).build()`.
    pub fn from_client(client: JsonRpcClient) -> Self {
        Self::builder().clients([client]).build()
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

    /// Delay between async transaction status polls, as configured on the builder.
    pub(crate) fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Overall deadline for async transaction status polling, as configured on the builder.
    pub(crate) fn poll_timeout(&self) -> Option<Duration> {
        self.poll_timeout
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
            let strategy = FixedInterval::new(self.retry_backoff).take(self.retries_per_endpoint);
            let request_timeout = self.request_timeout;
            let retry = RetryIf::start(
                strategy,
                || async {
                    match tokio::time::timeout(request_timeout, client.call(method)).await {
                        Ok(result) => result,
                        // A hung endpoint must not block forever: surface the elapsed
                        // timer as the existing retryable "request timeout" status error.
                        Err(_) => Err(JsonRpcError::ServerError(
                            JsonRpcServerError::ResponseStatusError(
                                JsonRpcServerResponseStatusError::TimeoutError,
                            ),
                        )),
                    }
                },
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
    use crate::signer::ExposeAccountId;
    use crate::testkit::{
        access_key_result, block_result, block_result_with_hash, final_outcome, full_access,
        healthy_mock, in_memory_signer, invalid_nonce_tx_error, jsonrpc_client, jsonrpc_error_body,
        jsonrpc_ok, slow_mock, timeout_mock, timeout_mock_with_attempts, transport_error,
        tx_pending_result, tx_result, wrong_kind_query_result,
    };
    use mockito::Server;
    use near_jsonrpc_client::errors::JsonRpcServerResponseStatusError;
    use near_jsonrpc_client::methods::health::RpcHealthRequest;
    use near_jsonrpc_primitives::types::query::RpcQueryError;
    use near_primitives::errors::InvalidTxError;
    use near_primitives::types::{BlockReference, ShardId};
    use near_primitives::views::{AccessKeyPermissionView, FinalExecutionOutcomeWithReceiptView};

    /// A single mock endpoint that dispatches by JSON-RPC method name.
    async fn method_server(
        query_result: serde_json::Value,
        block_result: serde_json::Value,
        tx_result: serde_json::Value,
    ) -> (mockito::ServerGuard, mockito::Mock) {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                let result = match body["method"].as_str() {
                    Some("query") => query_result.clone(),
                    Some("block") => block_result.clone(),
                    Some("send_tx") | Some("tx") => tx_result.clone(),
                    _ => serde_json::Value::Null,
                };
                jsonrpc_ok(result)
            })
            .expect_at_least(1)
            .create_async()
            .await;
        (server, mock)
    }

    #[tokio::test]
    async fn call_with_retry_fails_over_to_healthy_endpoint() {
        let mut failing = Server::new_async().await;
        let mut healthy = Server::new_async().await;

        // Every attempt against the first endpoint returns a retryable HTTP 408.
        let failing_mock = failing
            .mock("POST", "/")
            .with_status(408)
            .expect(3) // default: 2 retries plus the initial attempt
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

        let client = Client::builder()
            .clients(vec![
                jsonrpc_client(&failing.url()),
                jsonrpc_client(&healthy.url()),
            ])
            .build();

        // Any error is retryable for this read-only query, so the failing endpoint
        // gets the default 3 attempts before failing over.
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

    #[tokio::test]
    async fn status_timeout_is_retried_with_status_error_classifier() {
        let mut server = Server::new_async().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let timeout_body = jsonrpc_error_body(RpcTransactionError::TimeoutError(None));
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
        let body = jsonrpc_error_body(RpcTransactionError::InvalidTransaction {
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

    #[tokio::test]
    async fn builder_clients_fail_over_to_second_endpoint() {
        let mut first = Server::new_async().await;
        let mut second = Server::new_async().await;
        let failing = timeout_mock(&mut first).await;
        let healthy = healthy_mock(&mut second).await;

        let client = Client::builder()
            .clients(vec![
                jsonrpc_client(&first.url()),
                jsonrpc_client(&second.url()),
            ])
            .build();
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
    async fn builder_endpoints_fail_over_to_last_endpoint() {
        let mut first = Server::new_async().await;
        let mut second = Server::new_async().await;
        let mut third = Server::new_async().await;
        let failing_first = timeout_mock(&mut first).await;
        let failing_second = timeout_mock(&mut second).await;
        let healthy = healthy_mock(&mut third).await;

        let client = Client::builder()
            .endpoints(&[
                first.url().as_str(),
                second.url().as_str(),
                third.url().as_str(),
            ])
            .build();
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

    #[test]
    fn client_accessors_and_conversion() {
        let client = Client::new("http://127.0.0.1:1");
        assert_eq!(client.endpoints().len(), 1);
        assert_eq!(client.current_endpoint_index(), 0);
        assert_eq!(client.rpc_addr(), "http://127.0.0.1:1");
        assert!(std::ptr::eq(
            client.current_endpoint(),
            &client.endpoints()[0]
        ));
        assert_eq!(client.inner().server_addr(), "http://127.0.0.1:1");

        let converted: JsonRpcClient = client.clone().into();
        assert_eq!(converted.server_addr(), client.rpc_addr());

        let mut client = client;
        assert_eq!(client.inner_mut().server_addr(), "http://127.0.0.1:1");
    }

    #[test]
    fn builder_defaults() {
        let client = Client::builder().endpoint("http://127.0.0.1:1").build();
        assert_eq!(client.endpoints().len(), 1);
        assert_eq!(client.retry_backoff, Duration::from_millis(50));
        assert_eq!(client.retries_per_endpoint, 2);
        assert_eq!(client.request_timeout, Duration::from_secs(30));
        assert_eq!(client.poll_interval, Duration::from_millis(300));
        assert_eq!(client.poll_timeout, None);
    }

    #[test]
    fn builder_endpoints_append_in_order() {
        let client = Client::builder()
            .endpoint("http://127.0.0.1:1")
            .endpoint("http://127.0.0.1:2")
            .endpoints(&["http://127.0.0.1:3", "http://127.0.0.1:4"])
            .build();
        let addrs: Vec<_> = client
            .endpoints()
            .iter()
            .map(|client| client.server_addr())
            .collect();
        assert_eq!(
            addrs,
            [
                "http://127.0.0.1:1",
                "http://127.0.0.1:2",
                "http://127.0.0.1:3",
                "http://127.0.0.1:4",
            ]
        );
    }

    #[test]
    fn builder_clients_append_to_existing_endpoints() {
        let client = Client::builder()
            .endpoint("http://127.0.0.1:1")
            .clients(vec![
                jsonrpc_client("http://127.0.0.1:2"),
                jsonrpc_client("http://127.0.0.1:3"),
            ])
            .build();
        let addrs: Vec<_> = client
            .endpoints()
            .iter()
            .map(|client| client.server_addr())
            .collect();
        assert_eq!(
            addrs,
            [
                "http://127.0.0.1:1",
                "http://127.0.0.1:2",
                "http://127.0.0.1:3",
            ]
        );
    }

    #[test]
    fn builder_retry_settings_are_stored_on_client() {
        let client = Client::builder()
            .endpoint("http://127.0.0.1:1")
            .retry_backoff(Duration::from_millis(7))
            .retries_per_endpoint(4)
            .build();
        assert_eq!(client.retry_backoff, Duration::from_millis(7));
        assert_eq!(client.retries_per_endpoint, 4);
    }

    #[test]
    fn builder_timeout_settings_are_stored_on_client() {
        let client = Client::builder()
            .endpoint("http://127.0.0.1:1")
            .request_timeout(Duration::from_secs(2))
            .poll_interval(Duration::from_millis(7))
            .poll_timeout(Some(Duration::from_secs(5)))
            .build();
        assert_eq!(client.request_timeout, Duration::from_secs(2));
        assert_eq!(client.poll_interval(), Duration::from_millis(7));
        assert_eq!(client.poll_timeout(), Some(Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn request_timeout_expires_every_attempt() {
        let mut server = Server::new_async().await;
        // The response arrives after 60 ms, well past the 20 ms request timeout.
        let mock = slow_mock(&mut server, Duration::from_millis(60), 3).await;
        let client = Client::builder()
            .request_timeout(Duration::from_millis(20))
            // Space retries past the mock's 60 ms delay so the single-threaded
            // mockito server is free to register every attempt.
            .retry_backoff(Duration::from_millis(80))
            .endpoint(&server.url())
            .build();

        let result = client.send_query(&RpcHealthRequest).await;
        assert!(
            matches!(
                result,
                Err(JsonRpcError::ServerError(
                    JsonRpcServerError::ResponseStatusError(
                        JsonRpcServerResponseStatusError::TimeoutError
                    )
                ))
            ),
            "unexpected error: {result:?}"
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn request_timeout_fails_over_to_healthy_endpoint() {
        let mut slow = Server::new_async().await;
        let mut healthy = Server::new_async().await;
        let slow_mock = slow_mock(&mut slow, Duration::from_millis(60), 3).await;
        let healthy_mock = healthy_mock(&mut healthy).await;
        let client = Client::builder()
            .request_timeout(Duration::from_millis(20))
            .retry_backoff(Duration::from_millis(80))
            .endpoints(&[slow.url().as_str(), healthy.url().as_str()])
            .build();

        assert!(client.send_query(&RpcHealthRequest).await.is_ok());
        assert_eq!(client.current_endpoint_index(), 1);
        slow_mock.assert_async().await;
        healthy_mock.assert_async().await;
    }

    #[tokio::test]
    async fn configured_retries_are_used_before_failover() {
        let mut failing = Server::new_async().await;
        let mut healthy = Server::new_async().await;
        let failing_mock = timeout_mock_with_attempts(&mut failing, 2).await;
        let healthy_mock = healthy_mock(&mut healthy).await;
        let client = Client::builder()
            .retry_backoff(Duration::ZERO)
            .retries_per_endpoint(1)
            .endpoints(&[failing.url().as_str(), healthy.url().as_str()])
            .build();

        // One retry plus the initial attempt on the failing endpoint, then failover.
        assert!(client.send_query(&RpcHealthRequest).await.is_ok());
        assert_eq!(client.current_endpoint_index(), 1);
        failing_mock.assert_async().await;
        healthy_mock.assert_async().await;
    }

    #[tokio::test]
    async fn zero_retries_makes_exactly_one_attempt() {
        let mut server = Server::new_async().await;
        let mock = timeout_mock_with_attempts(&mut server, 1).await;
        let client = Client::builder()
            .retry_backoff(Duration::ZERO)
            .retries_per_endpoint(0)
            .endpoint(&server.url())
            .build();

        // Zero retries means only the initial attempt; no failover to another
        // endpoint is possible with a single configured endpoint.
        assert!(client.send_query(&RpcHealthRequest).await.is_err());
        assert_eq!(client.current_endpoint_index(), 0);
        mock.assert_async().await;
    }

    #[test]
    #[should_panic(expected = "at least one RPC endpoint is required")]
    fn builder_requires_an_endpoint() {
        Client::builder().build();
    }

    #[test]
    #[should_panic(expected = "at least one RPC endpoint is required")]
    fn empty_endpoints_are_rejected() {
        Client::builder().endpoints(&[]).build();
    }

    #[test]
    #[should_panic(expected = "at least one RPC endpoint is required")]
    fn empty_clients_are_rejected() {
        Client::builder().clients(Vec::new()).build();
    }

    #[test]
    fn query_error_classifier_distinguishes_handler_and_request_errors() {
        for err in [
            RpcQueryError::NoSyncedBlocks,
            RpcQueryError::UnavailableShard {
                requested_shard_id: ShardId::from(0),
            },
            RpcQueryError::GarbageCollectedBlock {
                block_height: 1,
                block_hash: CryptoHash::default(),
            },
            RpcQueryError::UnknownBlock {
                block_reference: BlockReference::Finality(Finality::Final),
            },
            RpcQueryError::InternalError {
                error_message: "boom".into(),
            },
        ] {
            let wrapped = server_error(JsonRpcServerError::HandlerError(err));
            assert!(!is_critical_query_error(&wrapped));
        }

        let invalid = server_error(JsonRpcServerError::HandlerError(
            RpcQueryError::InvalidAccount {
                requested_account_id: "alice.near".parse().unwrap(),
                block_height: 1,
                block_hash: CryptoHash::default(),
            },
        ));
        assert!(is_critical_query_error(&invalid));
        assert!(!is_critical_query_error(&transport_error::<RpcQueryError>()));
    }

    #[tokio::test]
    async fn nonce_cache_hit_increments_and_refetches_block() {
        let mut server = Server::new_async().await;
        let block_calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&block_calls);
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                match body["method"].as_str() {
                    Some("query") => jsonrpc_ok(access_key_result(5, full_access())),
                    Some("block") => {
                        // A different hash on every block request makes a stale
                        // (cached) block hash observable.
                        let call = counter.fetch_add(1, Ordering::SeqCst) as u8;
                        jsonrpc_ok(block_result_with_hash(CryptoHash::hash_bytes(&[call])))
                    }
                    _ => jsonrpc_ok(serde_json::Value::Null),
                }
            })
            .expect_at_least(3)
            .create_async()
            .await;
        // The cache miss takes the block hash from the access-key query; each
        // cache hit fetches a fresh block instead.
        let expected_hashes: Vec<_> = (0..2).map(|n| CryptoHash::hash_bytes(&[n])).collect();
        let client = Client::from_client(jsonrpc_client(&server.url()));
        let account: AccountId = "alice.near".parse().unwrap();
        let signer = in_memory_signer("alice.near");
        let pk = SignerExt::public_key(&signer);
        let cache_key = (account.clone(), pk.clone());

        let (nonce, hash, height) = client.fetch_nonce(&account, &pk).await.unwrap();
        assert_eq!(nonce, 6);
        assert_eq!(height, 10);
        assert_eq!(hash, CryptoHash::default());
        assert!(
            client
                .access_key_nonces
                .read()
                .await
                .contains_key(&cache_key)
        );

        // Cache hits increment the cached nonce but still fetch a fresh block.
        assert_eq!(
            client.fetch_nonce(&account, &pk).await.unwrap(),
            (7, expected_hashes[0], 10)
        );
        assert_eq!(
            client.fetch_nonce(&account, &pk).await.unwrap(),
            (8, expected_hashes[1], 10)
        );
        assert_eq!(block_calls.load(Ordering::SeqCst), 2);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn invalid_nonce_response_invalidates_cache() {
        let mut server = Server::new_async().await;
        let access_nonce = Arc::new(AtomicU64::new(5));
        let counter = Arc::clone(&access_nonce);
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                match body["method"].as_str() {
                    Some("query") => jsonrpc_ok(access_key_result(
                        counter.fetch_add(1, Ordering::SeqCst),
                        full_access(),
                    )),
                    Some("block") => jsonrpc_ok(block_result()),
                    _ => jsonrpc_ok(serde_json::Value::Null),
                }
            })
            .expect_at_least(3)
            .create_async()
            .await;

        let client = Client::from_client(jsonrpc_client(&server.url()));
        let account: AccountId = "alice.near".parse().unwrap();
        let signer = in_memory_signer("alice.near");
        let pk = SignerExt::public_key(&signer);
        let cache_key = (account.clone(), pk.clone());

        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 6);
        client
            .check_and_invalidate_cache(&cache_key, &Err(invalid_nonce_error()))
            .await;
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 7);

        client.invalidate_cache(&cache_key).await;
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 8);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn invalid_nonce_inside_outcome_invalidates_cache() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                match body["method"].as_str() {
                    Some("query") => jsonrpc_ok(access_key_result(5, full_access())),
                    Some("block") => jsonrpc_ok(block_result()),
                    _ => jsonrpc_ok(serde_json::Value::Null),
                }
            })
            .expect_at_least(2)
            .create_async()
            .await;

        let client = Client::from_client(jsonrpc_client(&server.url()));
        let account: AccountId = "alice.near".parse().unwrap();
        let pk = SignerExt::public_key(&in_memory_signer("alice.near"));
        let cache_key = (account.clone(), pk.clone());
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 6);

        let response = RpcTransactionResponse {
            final_execution_outcome: Some(FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
                final_outcome(
                    FinalExecutionStatus::SuccessValue(Vec::new()),
                    ExecutionStatusView::Unknown,
                    vec![ExecutionStatusView::Failure(invalid_nonce_tx_error())],
                ),
            )),
            final_execution_status: TxExecutionStatus::Final,
        };
        client
            .check_and_invalidate_cache(&cache_key, &Ok(response))
            .await;
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 6);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn delegate_action_invalid_nonce_in_outcome_invalidates_cache() {
        let mut server = Server::new_async().await;
        let access_nonce = Arc::new(AtomicU64::new(5));
        let counter = Arc::clone(&access_nonce);
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                match body["method"].as_str() {
                    // Jump by four so a refetch (nonce 9 -> 10) is
                    // distinguishable from a mere cache increment (nonce 7).
                    Some("query") => jsonrpc_ok(access_key_result(
                        counter.fetch_add(4, Ordering::SeqCst),
                        full_access(),
                    )),
                    Some("block") => jsonrpc_ok(block_result()),
                    _ => jsonrpc_ok(serde_json::Value::Null),
                }
            })
            .expect_at_least(2)
            .create_async()
            .await;

        let client = Client::from_client(jsonrpc_client(&server.url()));
        let account: AccountId = "alice.near".parse().unwrap();
        let pk = SignerExt::public_key(&in_memory_signer("alice.near"));
        let cache_key = (account.clone(), pk.clone());
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 6);

        let delegate_invalid = TxExecutionError::ActionError(ActionError {
            index: Some(0),
            kind: ActionErrorKind::DelegateActionInvalidNonce {
                delegate_nonce: 6,
                ak_nonce: 5,
            },
        });
        let response = RpcTransactionResponse {
            final_execution_outcome: Some(FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
                final_outcome(
                    FinalExecutionStatus::SuccessValue(Vec::new()),
                    ExecutionStatusView::SuccessValue(Vec::new()),
                    vec![ExecutionStatusView::Failure(delegate_invalid)],
                ),
            )),
            final_execution_status: TxExecutionStatus::Final,
        };
        client
            .check_and_invalidate_cache(&cache_key, &Ok(response))
            .await;
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 10);

        // An unrelated action error must leave the freshly cached nonce intact.
        let unrelated = TxExecutionError::ActionError(ActionError {
            index: None,
            kind: ActionErrorKind::DelegateActionNonceTooLarge {
                delegate_nonce: 10,
                upper_bound: 5,
            },
        });
        let response = RpcTransactionResponse {
            final_execution_outcome: Some(FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
                final_outcome(
                    FinalExecutionStatus::SuccessValue(Vec::new()),
                    ExecutionStatusView::SuccessValue(Vec::new()),
                    vec![ExecutionStatusView::Failure(unrelated)],
                ),
            )),
            final_execution_status: TxExecutionStatus::Final,
        };
        client
            .check_and_invalidate_cache(&cache_key, &Ok(response))
            .await;
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 11);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn fetch_tx_errs_collects_all_failure_sources() {
        let empty = RpcTransactionResponse {
            final_execution_outcome: None,
            final_execution_status: TxExecutionStatus::Final,
        };
        assert!(fetch_tx_errs(&empty).await.is_empty());

        let response = RpcTransactionResponse {
            final_execution_outcome: Some(FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
                final_outcome(
                    FinalExecutionStatus::Failure(invalid_nonce_tx_error()),
                    ExecutionStatusView::Failure(invalid_nonce_tx_error()),
                    vec![
                        ExecutionStatusView::Failure(invalid_nonce_tx_error()),
                        ExecutionStatusView::SuccessValue(Vec::new()),
                    ],
                ),
            )),
            final_execution_status: TxExecutionStatus::Final,
        };
        assert_eq!(fetch_tx_errs(&response).await.len(), 3);

        let with_receipts = RpcTransactionResponse {
            final_execution_outcome: Some(
                FinalExecutionOutcomeViewEnum::FinalExecutionOutcomeWithReceipt(
                    FinalExecutionOutcomeWithReceiptView {
                        final_outcome: final_outcome(
                            FinalExecutionStatus::Failure(invalid_nonce_tx_error()),
                            ExecutionStatusView::SuccessValue(Vec::new()),
                            vec![ExecutionStatusView::Failure(invalid_nonce_tx_error())],
                        ),
                        receipts: vec![],
                    },
                ),
            ),
            final_execution_status: TxExecutionStatus::Final,
        };
        assert_eq!(fetch_tx_errs(&with_receipts).await.len(), 2);
    }

    #[tokio::test]
    async fn access_key_happy_path_and_wrong_kind() {
        let (server, mock) = method_server(
            access_key_result(5, full_access()),
            block_result(),
            tx_pending_result(TxExecutionStatus::None),
        )
        .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        let account: AccountId = "alice.near".parse().unwrap();
        let pk = SignerExt::public_key(&in_memory_signer("alice.near"));
        let (view, hash, height) = client.access_key(&account, &pk).await.unwrap();
        assert_eq!(view.nonce, 5);
        assert_eq!(view.permission, AccessKeyPermissionView::FullAccess);
        assert_eq!(hash, CryptoHash::default());
        assert_eq!(height, 10);
        mock.assert_async().await;

        let (server, _) = method_server(
            wrong_kind_query_result(),
            block_result(),
            tx_pending_result(TxExecutionStatus::None),
        )
        .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        assert!(matches!(
            client.access_key(&account, &pk).await,
            Err(Error::RpcReturnedInvalidData(_))
        ));
    }

    #[tokio::test]
    async fn send_tx_once_signs_and_returns_outcome() {
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        let outcome = tx_result(
            FinalExecutionStatus::SuccessValue(b"ok".to_vec()),
            ExecutionStatusView::SuccessValue(b"ok".to_vec()),
            vec![],
            TxExecutionStatus::Final,
        );
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                seen.lock().unwrap().push(body.clone());
                match body["method"].as_str() {
                    Some("query") => jsonrpc_ok(access_key_result(5, full_access())),
                    Some("block") => jsonrpc_ok(block_result()),
                    Some("send_tx") => jsonrpc_ok(outcome.clone()),
                    other => panic!("unexpected RPC method: {other:?}"),
                }
            })
            .expect_at_least(1)
            .create_async()
            .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        let signer = in_memory_signer("alice.near");
        let receiver: AccountId = "bob.near".parse().unwrap();
        let outcome = client
            .send_tx_once(&signer, &receiver, vec![], TxExecutionStatus::Final)
            .await
            .unwrap();
        assert_eq!(
            outcome.status,
            FinalExecutionStatus::SuccessValue(b"ok".to_vec())
        );

        // Assert the signed transaction on the wire, not just the returned value.
        {
            let sent_bodies = bodies.lock().unwrap();
            let send_tx = sent_bodies
                .iter()
                .find(|body| body["method"] == "send_tx")
                .expect("send_tx request must be captured");
            let encoded = send_tx["params"]["signed_tx_base64"]
                .as_str()
                .expect("send_tx must carry a base64 signed transaction");
            let bytes = near_primitives::serialize::from_base64(encoded).unwrap();
            let signed: SignedTransaction = borsh::from_slice(&bytes).unwrap();
            match signed.transaction {
                Transaction::V0(tx) => {
                    assert_eq!(tx.receiver_id, receiver);
                    assert_eq!(&tx.signer_id, signer.account_id());
                    assert_ne!(tx.nonce, 0, "the signed transaction must carry a nonce");
                }
                other => panic!("unexpected transaction shape: {other:?}"),
            }
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn send_tx_once_handler_error_propagates_and_invalidates_cache() {
        let mut server = Server::new_async().await;
        let error_body = jsonrpc_error_body(RpcTransactionError::InvalidTransaction {
            context: InvalidTxError::InvalidNonce {
                tx_nonce: 6,
                ak_nonce: 5,
            },
        });
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                match body["method"].as_str() {
                    Some("query") => jsonrpc_ok(access_key_result(5, full_access())),
                    Some("block") => jsonrpc_ok(block_result()),
                    _ => error_body.as_bytes().to_vec(),
                }
            })
            .expect_at_least(2)
            .create_async()
            .await;

        let client = Client::from_client(jsonrpc_client(&server.url()));
        let signer = in_memory_signer("alice.near");
        let pk = SignerExt::public_key(&signer);
        let receiver: AccountId = "bob.near".parse().unwrap();
        let account = signer.account_id().clone();

        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 6);
        let err = client
            .send_tx_once(&signer, &receiver, vec![], TxExecutionStatus::Final)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::RpcTransactionError(_)));
        assert_eq!(client.fetch_nonce(&account, &pk).await.unwrap().0, 6);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn send_tx_async_returns_hash_and_propagates_handler_errors() {
        let (server, _) = method_server(
            access_key_result(5, full_access()),
            block_result(),
            tx_pending_result(TxExecutionStatus::None),
        )
        .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        let signer = in_memory_signer("alice.near");
        let receiver: AccountId = "bob.near".parse().unwrap();
        let hash = client
            .send_tx_async(&signer, &receiver, vec![])
            .await
            .unwrap();
        assert_ne!(hash, CryptoHash::default());

        let mut server = Server::new_async().await;
        let error_body = jsonrpc_error_body(RpcTransactionError::InvalidTransaction {
            context: InvalidTxError::InvalidNonce {
                tx_nonce: 6,
                ak_nonce: 5,
            },
        });
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                match body["method"].as_str() {
                    Some("query") => jsonrpc_ok(access_key_result(5, full_access())),
                    Some("block") => jsonrpc_ok(block_result()),
                    _ => error_body.as_bytes().to_vec(),
                }
            })
            .expect_at_least(1)
            .create_async()
            .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        assert!(
            client
                .send_tx_async(&signer, &receiver, vec![])
                .await
                .is_err()
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn status_tx_async_pending_ready_and_missing_outcome() {
        let account: AccountId = "alice.near".parse().unwrap();

        let (server, _) = method_server(
            serde_json::Value::Null,
            block_result(),
            tx_pending_result(TxExecutionStatus::Included),
        )
        .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        assert!(matches!(
            client
                .status_tx_async(&account, CryptoHash::default(), TxExecutionStatus::Final)
                .await,
            Err(Error::RpcTransactionPending)
        ));

        let (server, _) = method_server(
            serde_json::Value::Null,
            block_result(),
            tx_result(
                FinalExecutionStatus::SuccessValue(b"done".to_vec()),
                ExecutionStatusView::SuccessValue(b"done".to_vec()),
                vec![],
                TxExecutionStatus::Final,
            ),
        )
        .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        let outcome = client
            .status_tx_async(&account, CryptoHash::default(), TxExecutionStatus::Final)
            .await
            .unwrap();
        assert_eq!(
            outcome.status,
            FinalExecutionStatus::SuccessValue(b"done".to_vec())
        );

        let (server, _) = method_server(
            serde_json::Value::Null,
            block_result(),
            tx_pending_result(TxExecutionStatus::Final),
        )
        .await;
        let client = Client::from_client(jsonrpc_client(&server.url()));
        assert!(matches!(
            client
                .status_tx_async(&account, CryptoHash::default(), TxExecutionStatus::Final)
                .await,
            Err(Error::RpcReturnedInvalidData(_))
        ));
    }

    #[tokio::test]
    async fn failover_wraps_around_to_the_first_endpoint() {
        let mut first = Server::new_async().await;
        let mut second = Server::new_async().await;
        let first_attempts = Arc::new(AtomicUsize::new(0));
        let second_attempts = Arc::new(AtomicUsize::new(0));
        let first_count = Arc::clone(&first_attempts);
        let second_count = Arc::clone(&second_attempts);

        let error_body = serde_json::json!({
            "jsonrpc": "2.0",
            "error": {"code": -32603, "message": "temporarily unavailable"},
            "id": 1,
        })
        .to_string();
        let error_body_for_first = error_body.clone();
        let first_mock = first
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |_| {
                let count = first_count.fetch_add(1, Ordering::SeqCst);
                if count < 3 {
                    // Default: 2 retries plus the initial attempt.
                    error_body_for_first.as_bytes().to_vec()
                } else {
                    jsonrpc_ok(serde_json::Value::Null)
                }
            })
            .expect_at_least(4)
            .create_async()
            .await;
        let error_body_for_second = error_body.clone();
        let second_mock = second
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |_| {
                let count = second_count.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    jsonrpc_ok(serde_json::Value::Null)
                } else {
                    error_body_for_second.as_bytes().to_vec()
                }
            })
            .expect_at_least(4)
            .create_async()
            .await;

        let client = Client::builder()
            .endpoints(&[first.url().as_str(), second.url().as_str()])
            .build();
        assert!(client.send_query(&RpcHealthRequest).await.is_ok());
        assert_eq!(client.current_endpoint_index(), 1);

        // The second call starts at endpoint 1 and wraps around to endpoint 0.
        assert!(client.send_query(&RpcHealthRequest).await.is_ok());
        assert_eq!(client.current_endpoint_index(), 0);
        first_mock.assert_async().await;
        second_mock.assert_async().await;
    }

    #[tokio::test]
    async fn all_endpoints_failing_returns_last_error() {
        let mut first = Server::new_async().await;
        let mut second = Server::new_async().await;
        let failing_first = timeout_mock(&mut first).await;
        let failing_second = timeout_mock(&mut second).await;
        let client = Client::builder()
            .clients(vec![
                jsonrpc_client(&first.url()),
                jsonrpc_client(&second.url()),
            ])
            .build();

        assert!(client.send_query(&RpcHealthRequest).await.is_err());
        assert_eq!(client.current_endpoint_index(), 1);
        failing_first.assert_async().await;
        failing_second.assert_async().await;
    }
}
