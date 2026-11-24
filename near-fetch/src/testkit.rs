//! Shared fixtures for the crate's unit tests.
//!
//! This module is compiled only under `cfg(test)`. It collects the JSON-RPC
//! response builders, account/signer helpers, transaction/outcome builders, and
//! mock-server helpers that would otherwise be duplicated across the test
//! modules in `lib.rs`, `ops.rs`, `query.rs`, `result.rs`, `error.rs`, and
//! `signer.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use near_account_id::AccountId;
use near_crypto::{InMemorySigner, KeyType, SecretKey, Signature};
use near_jsonrpc_client::JsonRpcClient;
use near_jsonrpc_client::errors::{JsonRpcError, JsonRpcTransportRecvError, RpcTransportError};
use near_jsonrpc_client::methods::query::RpcQueryResponse;
use near_jsonrpc_client::methods::tx::RpcTransactionResponse;
use near_jsonrpc_primitives::message::Message;
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::errors::{InvalidTxError, TxExecutionError};
use near_primitives::hash::CryptoHash;
use near_primitives::types::Gas;
use near_primitives::views::{
    AccountView, BlockHeaderView, BlockView, ExecutionOutcomeView, ExecutionOutcomeWithIdView,
    ExecutionStatusView, FinalExecutionOutcomeView, FinalExecutionOutcomeViewEnum,
    FinalExecutionStatus, SignedTransactionView, TxExecutionStatus,
};
use near_token::NearToken;

use crate::Client;

// ---------------------------------------------------------------------------
// JSON-RPC response builders
// ---------------------------------------------------------------------------

/// Serialize a successful JSON-RPC response body.
pub(crate) fn jsonrpc_body(result: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "result": result, "id": 1}).to_string()
}

/// Serialize a successful JSON-RPC response body as bytes for mockito.
pub(crate) fn jsonrpc_ok(result: serde_json::Value) -> Vec<u8> {
    jsonrpc_body(result).into_bytes()
}

/// Serialize a JSON-RPC error body for a transaction handler error.
pub(crate) fn jsonrpc_error_body(err: RpcTransactionError) -> String {
    let rpc_error: near_jsonrpc_primitives::errors::RpcError = err.into();
    serde_json::json!({"jsonrpc": "2.0", "error": rpc_error, "id": 1}).to_string()
}

/// A minimal block response, matching the shape returned by the `block` method.
pub(crate) fn block_result() -> serde_json::Value {
    serde_json::to_value(BlockView {
        author: account("alice.near"),
        header: BlockHeaderView::default(),
        chunks: vec![],
    })
    .unwrap()
}

/// A block response with an identifiable hash, used to observe stale cache keys.
pub(crate) fn block_result_with_hash(hash: CryptoHash) -> serde_json::Value {
    let mut block: BlockView = serde_json::from_value(block_result()).unwrap();
    block.header.hash = hash;
    block.header.height = 10;
    serde_json::to_value(block).unwrap()
}

/// An access-key view payload returned by a `query` RPC request.
pub(crate) fn access_key_result(nonce: u64, permission: serde_json::Value) -> serde_json::Value {
    access_key_result_at_height(nonce, permission, 10)
}

/// An access-key view payload with an explicit block height.
pub(crate) fn access_key_result_at_height(
    nonce: u64,
    permission: serde_json::Value,
    block_height: u64,
) -> serde_json::Value {
    serde_json::json!({
        "nonce": nonce,
        "permission": permission,
        "block_height": block_height,
        "block_hash": "11111111111111111111111111111111",
    })
}

/// A `FullAccess` permission payload.
pub(crate) fn full_access() -> serde_json::Value {
    serde_json::Value::String("FullAccess".into())
}

/// A query result of the wrong kind for the typed query under test.
pub(crate) fn wrong_kind_query_result() -> serde_json::Value {
    serde_json::json!({
        "result": [],
        "logs": [],
        "block_height": 1,
        "block_hash": "11111111111111111111111111111111",
    })
}

/// A minimal `AccountView`, used as an unexpected query response kind.
pub(crate) fn account_view() -> AccountView {
    AccountView {
        amount: NearToken::from_near(1),
        locked: NearToken::ZERO,
        code_hash: CryptoHash::default(),
        storage_usage: 0,
        storage_paid_at: 0,
        global_contract_hash: None,
        global_contract_account_id: None,
        state: Default::default(),
        bootstrap_nonce: None,
    }
}

/// A typed query result of the wrong kind for the query under test.
pub(crate) fn wrong_kind_query_response() -> QueryResponseKind {
    QueryResponseKind::ViewAccount(account_view())
}

/// A typed query response with a fixed block height and hash.
pub(crate) fn query_response(kind: QueryResponseKind) -> RpcQueryResponse {
    RpcQueryResponse {
        kind,
        block_height: 9,
        block_hash: CryptoHash::default(),
    }
}

/// A transport-level JSON-RPC error, usable with any method's error type.
pub(crate) fn transport_error<E>() -> JsonRpcError<E> {
    JsonRpcError::TransportError(RpcTransportError::RecvError(
        JsonRpcTransportRecvError::UnexpectedServerResponse(Box::new(Message::notification(
            "test".into(),
            serde_json::Value::Null,
        ))),
    ))
}

// ---------------------------------------------------------------------------
// Account, signer, and client helpers
// ---------------------------------------------------------------------------

/// Parse an account id, panicking on invalid input.
pub(crate) fn account(id: &str) -> AccountId {
    id.parse().unwrap()
}

/// Build a fresh in-memory signer for `id`.
pub(crate) fn in_memory_signer(id: &str) -> InMemorySigner {
    let account_id = account(id);
    let secret_key = SecretKey::from_random(KeyType::ED25519);
    let public_key = secret_key.public_key();
    InMemorySigner {
        account_id,
        public_key,
        secret_key,
    }
}

/// A JSON-RPC client connected to `addr`, without issuing any request.
pub(crate) fn jsonrpc_client(addr: &str) -> JsonRpcClient {
    JsonRpcClient::new_client().connect(addr)
}

/// A high-level client connected to `addr`.
pub(crate) fn client_for_url(addr: &str) -> Client {
    Client::from_client(jsonrpc_client(addr))
}

/// A high-level client connected to a mockito server.
pub(crate) async fn client_for_server(server: &mockito::Server) -> Client {
    client_for_url(&server.url())
}

/// A client pointing at a closed port; used where no RPC request should occur.
pub(crate) fn dummy_client() -> Client {
    client_for_url("http://127.0.0.1:1")
}

// ---------------------------------------------------------------------------
// Transaction and outcome builders
// ---------------------------------------------------------------------------

/// The canonical `InvalidNonce` execution error used by failure-path tests.
pub(crate) fn invalid_nonce_tx_error() -> TxExecutionError {
    TxExecutionError::InvalidTxError(InvalidTxError::InvalidNonce {
        tx_nonce: 1,
        ak_nonce: 2,
    })
}

/// An execution outcome with configurable logs and burnt gas.
pub(crate) fn execution_outcome(
    status: ExecutionStatusView,
    logs: &[&str],
    gas: u64,
) -> ExecutionOutcomeWithIdView {
    ExecutionOutcomeWithIdView {
        proof: vec![],
        block_hash: CryptoHash::default(),
        id: CryptoHash::default(),
        outcome: ExecutionOutcomeView {
            logs: logs.iter().map(|s| (*s).to_string()).collect(),
            receipt_ids: vec![],
            gas_burnt: Gas::from_gas(gas),
            tokens_burnt: NearToken::ZERO,
            executor_id: account("alice.near"),
            status,
            metadata: Default::default(),
        },
    }
}

/// An execution outcome with no logs and one unit of gas.
pub(crate) fn outcome_with_id(status: ExecutionStatusView) -> ExecutionOutcomeWithIdView {
    execution_outcome(status, &[], 1)
}

/// A signed transaction view with fixed, predictable contents.
pub(crate) fn signed_transaction_view() -> SignedTransactionView {
    SignedTransactionView {
        signer_id: account("alice.near"),
        public_key: near_crypto::PublicKey::empty(KeyType::ED25519),
        nonce: 1,
        receiver_id: account("bob.near"),
        actions: vec![],
        _priority_fee: 0,
        signature: Signature::from_parts(KeyType::ED25519, &[0u8; 64]).unwrap(),
        hash: CryptoHash::default(),
        nonce_index: None,
        nonce_mode: None,
    }
}

/// Assemble a final execution outcome from its statuses.
pub(crate) fn final_outcome(
    status: FinalExecutionStatus,
    transaction_status: ExecutionStatusView,
    receipts: Vec<ExecutionStatusView>,
) -> FinalExecutionOutcomeView {
    FinalExecutionOutcomeView {
        status,
        transaction: signed_transaction_view(),
        transaction_outcome: outcome_with_id(transaction_status),
        receipts_outcome: receipts.into_iter().map(outcome_with_id).collect(),
    }
}

/// A `tx` RPC response carrying a final execution outcome.
pub(crate) fn tx_result(
    status: FinalExecutionStatus,
    transaction_status: ExecutionStatusView,
    receipts: Vec<ExecutionStatusView>,
    finality: TxExecutionStatus,
) -> serde_json::Value {
    serde_json::to_value(RpcTransactionResponse {
        final_execution_outcome: Some(FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
            final_outcome(status, transaction_status, receipts),
        )),
        final_execution_status: finality,
    })
    .unwrap()
}

/// A `tx` RPC response whose transaction succeeded with `b"ok"`.
pub(crate) fn tx_success_result(
    status: FinalExecutionStatus,
    finality: TxExecutionStatus,
) -> serde_json::Value {
    tx_result(
        status,
        ExecutionStatusView::SuccessValue(b"ok".to_vec()),
        vec![],
        finality,
    )
}

/// A `tx` RPC response without a final execution outcome.
pub(crate) fn tx_pending_result(finality: TxExecutionStatus) -> serde_json::Value {
    serde_json::to_value(RpcTransactionResponse {
        final_execution_outcome: None,
        final_execution_status: finality,
    })
    .unwrap()
}

// ---------------------------------------------------------------------------
// Mock server helpers
// ---------------------------------------------------------------------------

/// Request bodies captured by a mock server.
pub(crate) type CapturedBodies = Arc<Mutex<Vec<serde_json::Value>>>;

/// A mock endpoint that serves access-key, block, and transaction responses.
pub(crate) async fn tx_result_server(
    tx_result: serde_json::Value,
) -> (mockito::ServerGuard, mockito::Mock, CapturedBodies) {
    let mut server = mockito::Server::new_async().await;
    let block = block_result();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    let mock = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |request| {
            let body: serde_json::Value = serde_json::from_slice(request.body().unwrap()).unwrap();
            seen.lock().unwrap().push(body.clone());
            match body["method"].as_str() {
                Some("query") => jsonrpc_ok(access_key_result(5, full_access())),
                Some("block") => jsonrpc_ok(block.clone()),
                _ => jsonrpc_ok(tx_result.clone()),
            }
        })
        .expect_at_least(1)
        .create_async()
        .await;
    (server, mock, bodies)
}

/// A mock endpoint that answers with an empty successful response once.
pub(crate) async fn healthy_mock(server: &mut mockito::Server) -> mockito::Mock {
    server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"jsonrpc":"2.0","result":null,"id":1}"#)
        .expect(1)
        .create_async()
        .await
}

/// A mock endpoint that reports HTTP 408 for the default retry budget.
pub(crate) async fn timeout_mock(server: &mut mockito::Server) -> mockito::Mock {
    timeout_mock_with_attempts(server, 3).await
}

/// A mock endpoint that reports HTTP 408 for `attempts` requests.
pub(crate) async fn timeout_mock_with_attempts(
    server: &mut mockito::Server,
    attempts: usize,
) -> mockito::Mock {
    server
        .mock("POST", "/")
        .with_status(408)
        .expect(attempts)
        .create_async()
        .await
}

/// A mock endpoint whose response arrives only after `delay`.
///
/// Mockito serves on its own thread/runtime, so this sleep does not block the
/// client's timer that enforces `request_timeout`.
pub(crate) async fn slow_mock(
    server: &mut mockito::Server,
    delay: Duration,
    attempts: usize,
) -> mockito::Mock {
    server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |_| {
            std::thread::sleep(delay);
            jsonrpc_ok(serde_json::Value::Null)
        })
        .expect(attempts)
        .create_async()
        .await
}
