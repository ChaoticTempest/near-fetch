//! Error and result types used throughout `near-fetch`.

use near_jsonrpc_client::errors::JsonRpcError;
use near_jsonrpc_client::methods::broadcast_tx_async::RpcBroadcastTxAsyncError;
use near_jsonrpc_primitives::types::blocks::RpcBlockError;
use near_jsonrpc_primitives::types::chunks::RpcChunkError;
use near_jsonrpc_primitives::types::query::RpcQueryError;
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::errors::TxExecutionError;

/// Convenience result alias that defaults to this crate's [`Error`] type.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors that can occur when interacting with the NEAR network through this crate.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// A JSON-RPC error returned while requesting a block.
    #[error(transparent)]
    RpcBlockError(Box<JsonRpcError<RpcBlockError>>),
    /// A JSON-RPC error returned while performing a query.
    #[error(transparent)]
    RpcQueryError(Box<JsonRpcError<RpcQueryError>>),
    /// A JSON-RPC error returned while requesting a chunk.
    #[error(transparent)]
    RpcChunkError(Box<JsonRpcError<RpcChunkError>>),
    /// A JSON-RPC error returned while sending a transaction or checking its status.
    #[error(transparent)]
    RpcTransactionError(Box<JsonRpcError<RpcTransactionError>>),
    /// A JSON-RPC error returned while submitting an asynchronous transaction.
    #[error(transparent)]
    RpcTransactionAsyncError(Box<JsonRpcError<RpcBroadcastTxAsyncError>>),
    /// The submitted transaction has not completed yet.
    #[error("transaction has not completed yet")]
    RpcTransactionPending,
    /// The RPC returned data that could not be interpreted in the expected form.
    #[error("invalid data returned: {0}")]
    RpcReturnedInvalidData(String),
    /// Catch all RPC error. This is usually resultant from query calls.
    #[error("rpc: {0}")]
    Rpc(Box<dyn std::error::Error + Send + Sync>),
    /// An error raised while the network was executing the transaction.
    #[error(transparent)]
    TxExecution(Box<TxExecutionError>),
    /// The transaction reached a status that is not final, such as `Started`.
    #[error("tx_status={0}")]
    TxStatus(&'static str),

    /// A failure to serialize arguments or deserialize a JSON response.
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    /// A failure to decode base64-encoded data returned by the network.
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    /// An underlying I/O failure, such as one raised by Borsh serialization.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Invalid arguments were passed to a builder or constructor.
    #[error("invalid args were passed: {0}")]
    InvalidArgs(&'static str),
}

impl From<JsonRpcError<RpcBlockError>> for Error {
    fn from(err: JsonRpcError<RpcBlockError>) -> Self {
        Error::RpcBlockError(Box::new(err))
    }
}

impl From<JsonRpcError<RpcQueryError>> for Error {
    fn from(err: JsonRpcError<RpcQueryError>) -> Self {
        Error::RpcQueryError(Box::new(err))
    }
}

impl From<JsonRpcError<RpcChunkError>> for Error {
    fn from(err: JsonRpcError<RpcChunkError>) -> Self {
        Error::RpcChunkError(Box::new(err))
    }
}

impl From<JsonRpcError<RpcTransactionError>> for Error {
    fn from(err: JsonRpcError<RpcTransactionError>) -> Self {
        Error::RpcTransactionError(Box::new(err))
    }
}

impl From<JsonRpcError<RpcBroadcastTxAsyncError>> for Error {
    fn from(err: JsonRpcError<RpcBroadcastTxAsyncError>) -> Self {
        Error::RpcTransactionAsyncError(Box::new(err))
    }
}
