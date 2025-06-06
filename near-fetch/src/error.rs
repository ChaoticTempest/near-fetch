use near_jsonrpc_client::errors::JsonRpcError;
use near_jsonrpc_client::methods::broadcast_tx_async::RpcBroadcastTxAsyncError;
use near_jsonrpc_primitives::types::blocks::RpcBlockError;
use near_jsonrpc_primitives::types::chunks::RpcChunkError;
use near_jsonrpc_primitives::types::query::RpcQueryError;
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::errors::TxExecutionError;

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    RpcBlockError(Box<JsonRpcError<RpcBlockError>>),
    #[error(transparent)]
    RpcQueryError(Box<JsonRpcError<RpcQueryError>>),
    #[error(transparent)]
    RpcChunkError(Box<JsonRpcError<RpcChunkError>>),
    #[error(transparent)]
    RpcTransactionError(Box<JsonRpcError<RpcTransactionError>>),
    #[error(transparent)]
    RpcTransactionAsyncError(Box<JsonRpcError<RpcBroadcastTxAsyncError>>),
    #[error("transaction has not completed yet")]
    RpcTransactionPending,
    #[error("invalid data returned: {0}")]
    RpcReturnedInvalidData(String),
    /// Catch all RPC error. This is usually resultant from query calls.
    #[error("rpc: {0}")]
    Rpc(Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    TxExecution(Box<TxExecutionError>),
    #[error("tx_status={0}")]
    TxStatus(&'static str),

    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
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
