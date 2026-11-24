//! Error and result types used throughout `near-fetch`.

use std::time::Duration;

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
    /// The transaction did not complete within the configured polling timeout.
    #[error("timed out waiting for transaction after {0:?}")]
    PollTimeout(Duration),
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

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use base64::Engine as _;
    use base64::engine::general_purpose;
    use near_jsonrpc_client::errors::{JsonRpcTransportRecvError, RpcTransportError};
    use near_jsonrpc_primitives::message::Message;
    use near_primitives::errors::InvalidTxError;

    use super::*;

    fn transport_error<E>() -> JsonRpcError<E> {
        JsonRpcError::TransportError(RpcTransportError::RecvError(
            JsonRpcTransportRecvError::UnexpectedServerResponse(Box::new(Message::notification(
                "test".into(),
                serde_json::Value::Null,
            ))),
        ))
    }

    fn tx_execution_error() -> TxExecutionError {
        TxExecutionError::InvalidTxError(InvalidTxError::InvalidNonce {
            tx_nonce: 1,
            ak_nonce: 2,
        })
    }

    #[derive(Debug)]
    struct SourceLeaf(&'static str);

    impl std::fmt::Display for SourceLeaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for SourceLeaf {}

    #[derive(Debug)]
    struct SourceWrapper(SourceLeaf);

    impl std::fmt::Display for SourceWrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("wrapper")
        }
    }
    impl std::error::Error for SourceWrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn from_jsonrpc_errors_wrap_in_the_matching_variant() {
        assert!(matches!(
            Error::from(transport_error::<RpcBlockError>()),
            Error::RpcBlockError(_)
        ));
        assert!(matches!(
            Error::from(transport_error::<RpcQueryError>()),
            Error::RpcQueryError(_)
        ));
        assert!(matches!(
            Error::from(transport_error::<RpcChunkError>()),
            Error::RpcChunkError(_)
        ));
        assert!(matches!(
            Error::from(transport_error::<RpcTransactionError>()),
            Error::RpcTransactionError(_)
        ));
        assert!(matches!(
            Error::from(transport_error::<RpcBroadcastTxAsyncError>()),
            Error::RpcTransactionAsyncError(_)
        ));
    }

    #[test]
    fn display_messages_for_every_variant() {
        let cases: Vec<(Error, &str)> = vec![
            (
                Error::RpcBlockError(Box::new(transport_error())),
                "unexpected server response",
            ),
            (
                Error::RpcQueryError(Box::new(transport_error())),
                "unexpected server response",
            ),
            (
                Error::RpcChunkError(Box::new(transport_error())),
                "unexpected server response",
            ),
            (
                Error::RpcTransactionError(Box::new(transport_error())),
                "unexpected server response",
            ),
            (
                Error::RpcTransactionAsyncError(Box::new(transport_error())),
                "unexpected server response",
            ),
            (
                Error::RpcTransactionPending,
                "transaction has not completed yet",
            ),
            (
                Error::PollTimeout(Duration::from_millis(5)),
                "timed out waiting for transaction after 5ms",
            ),
            (
                Error::RpcReturnedInvalidData("bad payload".into()),
                "invalid data returned: bad payload",
            ),
            (
                Error::Rpc(Box::new(std::io::Error::other("boom"))),
                "rpc: boom",
            ),
            (
                Error::TxExecution(Box::new(tx_execution_error())),
                "must be larger",
            ),
            (Error::TxStatus("Started"), "tx_status=Started"),
            (
                Error::Serialization(serde_json::from_str::<u8>("nope").unwrap_err()),
                "expected ident",
            ),
            (
                Error::Base64(general_purpose::STANDARD.decode("!!!").unwrap_err()),
                "Invalid symbol",
            ),
            (Error::Io(std::io::Error::other("disk")), "disk"),
            (
                Error::InvalidArgs("missing id"),
                "invalid args were passed: missing id",
            ),
        ];

        for (error, expected) in cases {
            let rendered = error.to_string();
            assert!(
                rendered.contains(expected),
                "display {rendered:?} did not contain {expected:?}"
            );
        }
    }

    #[test]
    fn source_is_present_only_for_transparent_wrappers() {
        use general_purpose::STANDARD;

        // A transparent variant must expose a source when its inner error has
        // one; the message must survive the delegation unchanged.
        let nested = Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            SourceWrapper(SourceLeaf("inner disk failure")),
        ));
        let source = nested
            .source()
            .unwrap_or_else(|| panic!("{nested:?} must expose its wrapped source"));
        assert_eq!(source.to_string(), "inner disk failure");

        // Errors whose inner error is itself a leaf expose no source. The
        // transport errors chain down to `UnexpectedServerResponse`, which has
        // no source of its own.
        let leaves = vec![
            Error::from(transport_error::<RpcBlockError>()),
            Error::from(transport_error::<RpcQueryError>()),
            Error::from(transport_error::<RpcChunkError>()),
            Error::from(transport_error::<RpcTransactionError>()),
            Error::from(transport_error::<RpcBroadcastTxAsyncError>()),
            Error::RpcTransactionPending,
            Error::PollTimeout(Duration::from_millis(5)),
            Error::RpcReturnedInvalidData("bad".into()),
            Error::Rpc(Box::new(std::io::Error::other("boom"))),
            Error::TxExecution(Box::new(tx_execution_error())),
            Error::TxStatus("Started"),
            Error::Serialization(serde_json::from_str::<u8>("x").unwrap_err()),
            Error::Base64(STANDARD.decode("!!!").unwrap_err()),
            Error::Io(std::io::Error::other("disk")),
            Error::InvalidArgs("bad"),
        ];
        for error in leaves {
            assert!(
                error.source().is_none(),
                "{error:?} unexpectedly has a source: {:?}",
                error.source()
            );
        }
    }
}
