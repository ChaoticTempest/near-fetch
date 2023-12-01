//! This module defines a bunch of internal types used solely for querying into RPC
//! methods to retrieve info about what's on the chain. Note that the types defined
//! are exposed as-is for users to reference in their own functions or structs as
//! needed. These types cannot be created outside of near_fetch.

use std::fmt::{Debug, Display};

use near_account_id::AccountId;
use near_crypto::PublicKey;
use near_jsonrpc_client::methods::query::RpcQueryResponse;
use near_jsonrpc_client::methods::{self, RpcMethod};
use near_jsonrpc_primitives::types::chunks::ChunkReference;
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_primitives::borsh;
use near_primitives::hash::CryptoHash;
use near_primitives::types::{BlockHeight, BlockId, BlockReference, Finality, ShardId, StoreKey};
use near_primitives::views::{
    AccessKeyList, AccessKeyView, AccountView, BlockView, CallResult, ChunkView,
    FinalExecutionOutcomeView, QueryRequest, ViewStateResult,
};
use near_token::NearToken;

use crate::ops::Function;
use crate::{Client, Error, Result};

/// Intenral type used to represent a boxed future.
pub(crate) type BoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// `Query` object allows creating queries into the network of our choice. This object is
/// usually given from making calls from other functions such as [`view_state`].
///
/// [`view_state`]: crate::worker::Worker::view_state
pub struct Query<'a, T> {
    pub(crate) method: T,
    pub(crate) client: &'a Client,
    pub(crate) block_ref: Option<BlockReference>,
}

impl<'a, T> Query<'a, T> {
    pub(crate) fn new(client: &'a Client, method: T) -> Self {
        Self {
            method,
            client,
            block_ref: None,
        }
    }

    /// Specify at which block height to query from. Note that only archival
    /// networks will have the full history while networks like mainnet or testnet will
    /// only have the history from 5 or less epochs ago.
    pub fn block_height(mut self, height: BlockHeight) -> Self {
        self.block_ref = Some(BlockId::Height(height).into());
        self
    }

    /// Specify at which block hash to query from. Note that only archival
    /// networks will have the full history while networks like mainnet or testnet will
    /// only have the history from 5 or less epochs ago.
    pub fn block_hash(mut self, hash: CryptoHash) -> Self {
        self.block_ref = Some(BlockId::Hash(near_primitives::hash::CryptoHash(hash.0)).into());
        self
    }
}

// Constrained to RpcQueryRequest, since methods like GasPrice only take block_id but not Finality.
impl<'a, T> Query<'a, T>
where
    T: ProcessQuery<Method = methods::query::RpcQueryRequest>,
{
    /// Specify at which block [`Finality`] to query from.
    pub fn finality(mut self, value: Finality) -> Self {
        self.block_ref = Some(value.into());
        self
    }
}

impl<'a, T, R> std::future::IntoFuture for Query<'a, T>
where
    T: ProcessQuery<Output = R> + Send + Sync + 'static,
    <T as ProcessQuery>::Method: RpcMethod + Debug + Send + Sync,
    <<T as ProcessQuery>::Method as RpcMethod>::Response: Debug + Send + Sync,
    <<T as ProcessQuery>::Method as RpcMethod>::Error: Debug + Display + Send + Sync,
{
    type Output = Result<R>;

    // TODO: boxed future required due to impl Trait as type alias being unstable. So once
    // https://github.com/rust-lang/rust/issues/63063 is resolved, we can move to that instead.
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let block_reference = self.block_ref.unwrap_or_else(BlockReference::latest);
            let resp = self
                .client
                .send(self.method.into_request(block_reference)?)
                .await
                .map_err(|err| Error::Rpc(err.into()))?;

            T::from_response(resp)
        })
    }
}

// Note: this trait is exposed publicly due to constraining with the impl offering `finality`.
/// Trait used as a converter from WorkspaceRequest to near-rpc request, and from near-rpc
/// response to a WorkspaceResult. Mostly used internally to facilitate syntax sugar for performing
/// RPC requests with async builders.
pub trait ProcessQuery {
    // TODO: associated default type is unstable. So for now, will require writing
    // the manual impls for query_request
    /// Method for doing the internal RPC request to the network of our choosing.
    type Method: RpcMethod;

    /// Expected output after performing a query. This is mainly to convert over
    /// the type from near-primitives to a workspace type.
    type Output;

    /// Convert into the Request object that is required to perform the RPC request.
    fn into_request(self, block_ref: BlockReference) -> Result<Self::Method>;

    /// Convert the response from the RPC request to a type of our choosing, mainly to conform
    /// to workspaces related types from the near-primitives or json types from the network.
    fn from_response(resp: <Self::Method as RpcMethod>::Response) -> Result<Self::Output>;
}

pub struct ViewFunction {
    pub(crate) account_id: AccountId,
    pub(crate) function: Function,
}

pub struct ViewCode {
    pub(crate) account_id: AccountId,
}

pub struct ViewAccount {
    pub(crate) account_id: AccountId,
}

pub struct ViewBlock;

pub struct ViewState {
    account_id: AccountId,
    prefix: Option<Vec<u8>>,
}

pub struct ViewAccessKey {
    pub(crate) account_id: AccountId,
    pub(crate) public_key: PublicKey,
}

pub struct ViewAccessKeyList {
    pub(crate) account_id: AccountId,
}

pub struct GasPrice;

impl ProcessQuery for ViewFunction {
    type Method = methods::query::RpcQueryRequest;
    type Output = CallResult;

    fn into_request(self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::CallFunction {
                account_id: self.account_id,
                method_name: self.function.name,
                args: self.function.args?.into(),
            },
        })
    }

    fn from_response(resp: RpcQueryResponse) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::CallResult(result) => Ok(result),
            _ => Err(Error::RpcReturnedInvalidData(
                "while querying account".into(),
            )),
        }
    }
}

// Specific builder methods attached to a ViewFunction.
impl Query<'_, ViewFunction> {
    /// Provide the arguments for the call. These args are serialized bytes from either
    /// a JSON or Borsh serializable set of arguments. To use the more specific versions
    /// with better quality of life, use `args_json` or `args_borsh`.
    pub fn args(mut self, args: Vec<u8>) -> Self {
        self.method.function = self.method.function.args(args);
        self
    }

    /// Similar to `args`, specify an argument that is JSON serializable and can be
    /// accepted by the equivalent contract. Recommend to use something like
    /// `serde_json::json!` macro to easily serialize the arguments.
    pub fn args_json<U: serde::Serialize>(mut self, args: U) -> Self {
        self.method.function = self.method.function.args_json(args);
        self
    }

    /// Similar to `args`, specify an argument that is borsh serializable and can be
    /// accepted by the equivalent contract.
    pub fn args_borsh<U: borsh::BorshSerialize>(mut self, args: U) -> Self {
        self.method.function = self.method.function.args_borsh(args);
        self
    }
}

impl ProcessQuery for ViewCode {
    type Method = methods::query::RpcQueryRequest;
    type Output = Vec<u8>;

    fn into_request(self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewCode {
                account_id: self.account_id,
            },
        })
    }

    fn from_response(resp: RpcQueryResponse) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::ViewCode(contract) => Ok(contract.code),
            _ => Err(Error::RpcReturnedInvalidData("while querying code".into())),
        }
    }
}

impl ProcessQuery for ViewAccount {
    type Method = methods::query::RpcQueryRequest;
    type Output = AccountView;

    fn into_request(self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewAccount {
                account_id: self.account_id,
            },
        })
    }

    fn from_response(resp: RpcQueryResponse) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::ViewAccount(account) => Ok(account),
            _ => Err(Error::RpcReturnedInvalidData(
                "while querying account".into(),
            )),
        }
    }
}

impl ProcessQuery for ViewBlock {
    type Method = methods::block::RpcBlockRequest;
    type Output = BlockView;

    fn into_request(self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method { block_reference })
    }

    fn from_response(view: BlockView) -> Result<Self::Output> {
        Ok(view)
    }
}

impl ProcessQuery for ViewState {
    type Method = methods::query::RpcQueryRequest;
    type Output = ViewStateResult;

    fn into_request(self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewState {
                account_id: self.account_id,
                prefix: StoreKey::from(self.prefix.map(Vec::from).unwrap_or_default()),
                include_proof: false,
            },
        })
    }

    fn from_response(resp: <Self::Method as RpcMethod>::Response) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::ViewState(state) => Ok(state),
            _ => Err(Error::RpcReturnedInvalidData("while querying state".into())),
        }
    }
}

impl<'a> Query<'a, ViewState> {
    pub(crate) fn view_state(client: &'a Client, id: &AccountId) -> Self {
        Self::new(
            client,
            ViewState {
                account_id: id.clone(),
                prefix: None,
            },
        )
    }

    /// Set the prefix for viewing the state.
    pub fn prefix(mut self, value: &[u8]) -> Self {
        self.method.prefix = Some(value.into());
        self
    }
}

impl ProcessQuery for ViewAccessKey {
    type Method = methods::query::RpcQueryRequest;
    type Output = AccessKeyView;

    fn into_request(self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewAccessKey {
                account_id: self.account_id,
                public_key: self.public_key,
            },
        })
    }

    fn from_response(resp: <Self::Method as RpcMethod>::Response) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::AccessKey(key) => Ok(key),
            _ => Err(Error::RpcReturnedInvalidData(
                "while querying access key".into(),
            )),
        }
    }
}

impl ProcessQuery for ViewAccessKeyList {
    type Method = methods::query::RpcQueryRequest;
    type Output = AccessKeyList;

    fn into_request(self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewAccessKeyList {
                account_id: self.account_id,
            },
        })
    }

    fn from_response(resp: <Self::Method as RpcMethod>::Response) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::AccessKeyList(keylist) => Ok(keylist),
            _ => Err(Error::RpcReturnedInvalidData(
                "while querying access keys".into(),
            )),
        }
    }
}

impl ProcessQuery for GasPrice {
    type Method = methods::gas_price::RpcGasPriceRequest;
    type Output = NearToken;

    fn into_request(self, block_ref: BlockReference) -> Result<Self::Method> {
        let block_id = match block_ref {
            // User provided input via `block_hash` or `block_height` functions.
            BlockReference::BlockId(block_id) => Some(block_id),
            // default case, set by `Query` struct via BlockReference::latest.
            BlockReference::Finality(_finality) => None,
            // Should not be reachable, unless code got changed.
            BlockReference::SyncCheckpoint(point) => {
                return Err(Error::RpcReturnedInvalidData(format!(
                    "Cannot supply sync checkpoint to gas price: {point:?}. Potential API bug?"
                )))
            }
        };

        Ok(Self::Method { block_id })
    }

    fn from_response(resp: <Self::Method as RpcMethod>::Response) -> Result<Self::Output> {
        Ok(NearToken::from_yoctonear(resp.gas_price))
    }
}

/// Query object used to query for chunk related details at a specific `ChunkReference` which
/// consists of either a chunk [`CryptoHash`], or a `BlockShardId` (which consists of [`ShardId`]
/// and either block [`CryptoHash`] or [`BlockHeight`]).
///
/// The default behavior where a `ChunkReference` is not supplied will use a `BlockShardId`
/// referencing the latest block `CryptoHash` with `ShardId` of 0.
pub struct QueryChunk<'a> {
    client: &'a Client,
    chunk_ref: Option<ChunkReference>,
}

impl<'a> QueryChunk<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            chunk_ref: None,
        }
    }

    /// Specify at which block hash and shard id to query the chunk from. Note that only
    /// archival networks will have the full history while networks like mainnet or testnet
    /// will only have the history from 5 or less epochs ago.
    pub fn block_hash_and_shard(mut self, hash: CryptoHash, shard_id: ShardId) -> Self {
        self.chunk_ref = Some(ChunkReference::BlockShardId {
            block_id: BlockId::Hash(near_primitives::hash::CryptoHash(hash.0)),
            shard_id,
        });
        self
    }

    /// Specify at which block height and shard id to query the chunk from. Note that only
    /// archival networks will have the full history while networks like mainnet or testnet
    /// will only have the history from 5 or less epochs ago.
    pub fn block_height_and_shard(mut self, height: BlockHeight, shard_id: ShardId) -> Self {
        self.chunk_ref = Some(ChunkReference::BlockShardId {
            block_id: BlockId::Height(height),
            shard_id,
        });
        self
    }

    /// Specify at which chunk hash to query the chunk from.
    pub fn chunk_hash(mut self, hash: CryptoHash) -> Self {
        self.chunk_ref = Some(ChunkReference::ChunkHash {
            chunk_id: near_primitives::hash::CryptoHash(hash.0),
        });
        self
    }
}

impl<'a> std::future::IntoFuture for QueryChunk<'a> {
    type Output = Result<ChunkView>;
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let chunk_reference = if let Some(chunk_ref) = self.chunk_ref {
                chunk_ref
            } else {
                // Use the latest block hash in the case the user doesn't supply the ChunkReference. Note that
                // shard_id 0 is used in the default case.
                let block_view = self.client.view_block().await?;
                ChunkReference::BlockShardId {
                    block_id: BlockId::Hash(block_view.header.hash),
                    shard_id: 0,
                }
            };

            let chunk_view = self
                .client
                .send(methods::chunk::RpcChunkRequest { chunk_reference })
                .await?;

            Ok(chunk_view)
        })
    }
}

impl Client {
    /// Call into a contract's view function. Returns a [`Query`] which allows us
    /// to specify further details like the arguments of the view call, or at what
    /// point in the chain we want to view.
    pub fn view(&self, contract_id: &AccountId, function: &str) -> Query<'_, ViewFunction> {
        Query::new(
            self,
            ViewFunction {
                account_id: contract_id.clone(),
                function: Function::new(function),
            },
        )
    }

    /// View the WASM code bytes of a contract on the network.
    pub fn view_code(&self, contract_id: &AccountId) -> Query<'_, ViewCode> {
        Query::new(
            self,
            ViewCode {
                account_id: contract_id.clone(),
            },
        )
    }

    /// View the state of a account/contract on the network. This will return the internal
    /// state of the account in the form of a map of key-value pairs; where STATE contains
    /// info on a contract's internal data.
    pub fn view_state(&self, contract_id: &AccountId) -> Query<'_, ViewState> {
        Query::view_state(self, contract_id)
    }

    /// View the block from the network. Supply additional parameters such as [`block_height`]
    /// or [`block_hash`] to get the block.
    ///
    /// [`block_height`]: Query::block_height
    /// [`block_hash`]: Query::block_hash
    pub fn view_block(&self) -> Query<'_, ViewBlock> {
        Query::new(self, ViewBlock)
    }

    /// View the chunk from the network once awaited. Supply additional parameters such as
    /// [`block_hash_and_shard`], [`block_height_and_shard`] or [`chunk_hash`] to get the
    /// chunk at a specific reference point. If none of those are supplied, the default
    /// reference point will be used, which will be the latest block_hash with a shard_id
    /// of 0.
    ///
    /// [`block_hash_and_shard`]: QueryChunk::block_hash_and_shard
    /// [`block_height_and_shard`]: QueryChunk::block_height_and_shard
    /// [`chunk_hash`]: QueryChunk::chunk_hash
    pub fn view_chunk(&self) -> QueryChunk<'_> {
        QueryChunk::new(self)
    }

    /// Views the [`AccessKey`] of the account specified by [`AccountId`] associated with
    /// the [`PublicKey`]
    ///
    /// [`AccessKey`]: crate::types::AccessKey
    pub fn view_access_key(&self, id: &AccountId, pk: &PublicKey) -> Query<'_, ViewAccessKey> {
        Query::new(
            self,
            ViewAccessKey {
                account_id: id.clone(),
                public_key: pk.clone(),
            },
        )
    }

    /// Views all the [`AccessKey`]s of the account specified by [`AccountId`]. This will
    /// return a list of [`AccessKey`]s along with the associated [`PublicKey`].
    ///
    /// [`AccessKey`]: crate::types::AccessKey
    pub fn view_access_keys(&self, id: &AccountId) -> Query<'_, ViewAccessKeyList> {
        Query::new(
            self,
            ViewAccessKeyList {
                account_id: id.clone(),
            },
        )
    }

    /// View account details of a specific account on the network.
    pub fn view_account(&self, account_id: &AccountId) -> Query<'_, ViewAccount> {
        Query::new(
            self,
            ViewAccount {
                account_id: account_id.clone(),
            },
        )
    }

    /// Fetches the latest gas price on the network.
    pub fn gas_price(&self) -> Query<'_, GasPrice> {
        Query::new(self, GasPrice)
    }

    /// Fetches the status of a transaction given the transaction hash.
    pub async fn tx_async_status(
        &self,
        sender_id: &AccountId,
        tx_hash: CryptoHash,
    ) -> Result<FinalExecutionOutcomeView> {
        self.rpc_client
            .call(methods::tx::RpcTransactionStatusRequest {
                transaction_info: methods::tx::TransactionInfo::TransactionId {
                    account_id: sender_id.clone(),
                    hash: tx_hash,
                },
            })
            .await
            .map_err(Into::into)
    }
}
