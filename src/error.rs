use near_jsonrpc_client::errors::JsonRpcError;
use near_jsonrpc_primitives::types::blocks::RpcBlockError;
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
    RpcTransactionError(#[from] JsonRpcError<RpcTransactionError>),
    #[error("invalid data returned: {0}")]
    RpcReturnedInvalidData(&'static str),

    #[error(transparent)]
    SerializeError(#[from] serde_json::Error),
    #[error("invalid args were passed: {0}")]
    InvalidArgs(&'static str),
}
