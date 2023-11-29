use near_jsonrpc_client::errors::JsonRpcError;
use near_jsonrpc_primitives::types::blocks::RpcBlockError;
use near_jsonrpc_primitives::types::chunks::RpcChunkError;
use near_jsonrpc_primitives::types::query::RpcQueryError;
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    RpcBlockError(#[from] JsonRpcError<RpcBlockError>),
    #[error(transparent)]
    RpcQueryError(#[from] JsonRpcError<RpcQueryError>),
    #[error(transparent)]
    RpcChunkError(#[from] JsonRpcError<RpcChunkError>),
    #[error(transparent)]
    RpcTransactionError(#[from] JsonRpcError<RpcTransactionError>),
    #[error("invalid data returned: {0}")]
    RpcReturnedInvalidData(String),
    /// Catch all RPC error. This is usually resultant from query calls.
    #[error("rpc: {0}")]
    Rpc(Box<dyn std::error::Error + Send + Sync>),

    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid args were passed: {0}")]
    InvalidArgs(&'static str),
}
