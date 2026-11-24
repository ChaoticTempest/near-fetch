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
    AccessKeyList, AccessKeyView, AccountView, BlockView, CallResult, ChunkView, QueryRequest,
    ViewStateResult,
};
use near_token::NearToken;

use crate::ops::Function;
use crate::{Client, Error, Result};

/// Internal type used to represent a boxed future.
pub(crate) type BoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// `Query` object allows creating queries into the network of our choice. This object is
/// usually given from making calls from other functions such as [`view_state`].
///
/// [`view_state`]: Client::view_state
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

impl<'a, T> Query<'a, T>
where
    T: ProcessQuery,
{
    /// Post-process the result of this query with the provided function, e.g. to
    /// extract or convert a field of the response.
    ///
    /// ## Example
    /// ```rust,no_run
    /// # use near_fetch::Client;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let client = Client::new("https://rpc.testnet.near.org");
    /// let price_in_near = client.gas_price().map(|p| p.to_string()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn map<U>(
        self,
        map: impl Fn(T::Output) -> U + Send + Sync + 'static,
    ) -> Query<'a, MappedQuery<T, impl Fn(T::Output) -> Result<U> + Send + Sync + 'static>> {
        Query {
            method: MappedQuery {
                inner: self.method,
                map: move |output| Ok(map(output)),
            },
            client: self.client,
            block_ref: self.block_ref,
        }
    }

    /// Post-process the result of this query with a fallible function, where
    /// failures are propagated as errors of this query.
    ///
    /// ## Example
    /// ```rust,no_run
    /// # use near_account_id::AccountId;
    /// # use near_fetch::Client;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let client = Client::new("https://rpc.testnet.near.org");
    /// # let contract: AccountId = "social.near".parse()?;
    /// let profile: serde_json::Value = client
    ///     .view(&contract, "get")
    ///     .args_json(serde_json::json!({ "keys": ["chaotictempest.near/profile/**"] }))
    ///     .and_then(|result| result.json())
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn and_then<U>(
        self,
        map: impl Fn(T::Output) -> Result<U> + Send + Sync + 'static,
    ) -> Query<'a, MappedQuery<T, impl Fn(T::Output) -> Result<U> + Send + Sync + 'static>> {
        Query {
            method: MappedQuery {
                inner: self.method,
                map,
            },
            client: self.client,
            block_ref: self.block_ref,
        }
    }
}

/// A query with a post-processing step applied to its result, created via
/// [`Query::map`] or [`Query::and_then`]. This is not intended to be constructed directly.
pub struct MappedQuery<T, F> {
    pub(crate) inner: T,
    pub(crate) map: F,
}

impl<T, F, U> ProcessQuery for MappedQuery<T, F>
where
    T: ProcessQuery,
    F: Fn(T::Output) -> Result<U>,
{
    type Method = T::Method;
    type Output = U;

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        T::build_request(&self.inner, block_reference)
    }

    fn process_response(
        &self,
        resp: <Self::Method as RpcMethod>::Response,
    ) -> Result<Self::Output> {
        T::process_response(&self.inner, resp).and_then(&self.map)
    }
}

/// Marker trait for RPC methods that accept a [`Finality`] as their block reference,
/// such as query and block requests. Methods like gas price only take a block id.
pub trait SupportsFinality: methods::RpcMethod {}

impl SupportsFinality for methods::query::RpcQueryRequest {}
impl SupportsFinality for methods::block::RpcBlockRequest {}

impl<'a, T> Query<'a, T>
where
    T: ProcessQuery,
    T::Method: SupportsFinality,
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
                .send_query(&self.method.build_request(block_reference)?)
                .await
                .map_err(|err| Error::Rpc(err.into()))?;

            self.method.process_response(resp)
        })
    }
}

// Note: this trait is exposed publicly due to constraining with the impl offering `finality`.
/// Trait used as a high level APIs for consistent usages of block reference. Mostly used
/// internally to facilitate syntax sugar for performing RPC requests with async builders.
pub trait ProcessQuery {
    // TODO: associated default type is unstable. So for now, will require writing
    // the manual impls for query_request
    /// Method for doing the internal RPC request to the network of our choosing.
    type Method: RpcMethod;

    /// Expected output after performing a query.
    type Output;

    /// Convert into the Request object that is required to perform the RPC request.
    fn build_request(&self, block_ref: BlockReference) -> Result<Self::Method>;

    /// Convert the response from the RPC request to a type of our choosing.
    fn process_response(&self, resp: <Self::Method as RpcMethod>::Response)
    -> Result<Self::Output>;
}

/// Query method for calling a view function on a contract, created by [`Client::view`].
#[derive(Clone)]
pub struct ViewFunction {
    pub(crate) account_id: AccountId,
    pub(crate) function: Function,
}

/// Query method for fetching a contract's WASM bytes, created by [`Client::view_code`].
#[derive(Clone)]
pub struct ViewCode {
    pub(crate) account_id: AccountId,
}

/// Query method for fetching account details, created by [`Client::view_account`].
#[derive(Clone)]
pub struct ViewAccount {
    pub(crate) account_id: AccountId,
}

/// Query method for fetching a block, created by [`Client::view_block`].
#[derive(Clone)]
pub struct ViewBlock;

/// Query method for viewing contract state, created by [`Client::view_state`].
#[derive(Clone)]
pub struct ViewState {
    account_id: AccountId,
    prefix: Option<Vec<u8>>,
}

/// Query method for fetching a single access key, created by [`Client::view_access_key`].
#[derive(Clone)]
pub struct ViewAccessKey {
    pub(crate) account_id: AccountId,
    pub(crate) public_key: PublicKey,
}

/// Query method for listing an account's access keys, created by [`Client::view_access_keys`].
#[derive(Clone)]
pub struct ViewAccessKeyList {
    pub(crate) account_id: AccountId,
}

/// Query method for fetching the current gas price, created by [`Client::gas_price`].
#[derive(Clone)]
pub struct GasPrice;

impl ProcessQuery for ViewFunction {
    type Method = methods::query::RpcQueryRequest;
    type Output = ViewResult;

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::CallFunction {
                account_id: self.account_id.clone(),
                method_name: self.function.name.clone(),
                args: self.function.clone().args?.into(),
            },
        })
    }

    fn process_response(&self, resp: RpcQueryResponse) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::CallResult(result) => Ok(result.into()),
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

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewCode {
                account_id: self.account_id.clone(),
            },
        })
    }

    fn process_response(&self, resp: RpcQueryResponse) -> Result<Self::Output> {
        match resp.kind {
            QueryResponseKind::ViewCode(contract) => Ok(contract.code),
            _ => Err(Error::RpcReturnedInvalidData("while querying code".into())),
        }
    }
}

impl ProcessQuery for ViewAccount {
    type Method = methods::query::RpcQueryRequest;
    type Output = AccountView;

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewAccount {
                account_id: self.account_id.clone(),
            },
        })
    }

    fn process_response(&self, resp: RpcQueryResponse) -> Result<Self::Output> {
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

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method { block_reference })
    }

    fn process_response(&self, view: BlockView) -> Result<Self::Output> {
        Ok(view)
    }
}

impl ProcessQuery for ViewState {
    type Method = methods::query::RpcQueryRequest;
    type Output = ViewStateResult;

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewState {
                account_id: self.account_id.clone(),
                prefix: StoreKey::from(self.prefix.clone().unwrap_or_default()),
                after_key: None,
                limit: None,
                include_proof: false,
            },
        })
    }

    fn process_response(
        &self,
        resp: <Self::Method as RpcMethod>::Response,
    ) -> Result<Self::Output> {
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

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewAccessKey {
                account_id: self.account_id.clone(),
                public_key: self.public_key.clone(),
            },
        })
    }

    fn process_response(
        &self,
        resp: <Self::Method as RpcMethod>::Response,
    ) -> Result<Self::Output> {
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

    fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
        Ok(Self::Method {
            block_reference,
            request: QueryRequest::ViewAccessKeyList {
                account_id: self.account_id.clone(),
                after_key: None,
                limit: None,
            },
        })
    }

    fn process_response(
        &self,
        resp: <Self::Method as RpcMethod>::Response,
    ) -> Result<Self::Output> {
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

    fn build_request(&self, block_ref: BlockReference) -> Result<Self::Method> {
        let block_id = match block_ref {
            // User provided input via `block_hash` or `block_height` functions.
            BlockReference::BlockId(block_id) => Some(block_id),
            // default case, set by `Query` struct via BlockReference::latest.
            BlockReference::Finality(_finality) => None,
            // Should not be reachable, unless code got changed.
            BlockReference::SyncCheckpoint(point) => {
                return Err(Error::RpcReturnedInvalidData(format!(
                    "Cannot supply sync checkpoint to gas price: {point:?}. Potential API bug?"
                )));
            }
        };

        Ok(Self::Method { block_id })
    }

    fn process_response(
        &self,
        resp: <Self::Method as RpcMethod>::Response,
    ) -> Result<Self::Output> {
        Ok(resp.gas_price)
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
                    shard_id: ShardId::from(0),
                }
            };

            let chunk_view = self
                .client
                .send_query(&methods::chunk::RpcChunkRequest { chunk_reference })
                .await?;

            Ok(chunk_view)
        })
    }
}

/// The result from a call into a View function. This contains the contents or
/// the results from the view function call itself. The consumer of this object
/// can choose how to deserialize its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ViewResult {
    /// Our result from our call into a view function.
    pub result: Vec<u8>,
    /// Logs generated from the view function.
    pub logs: Vec<String>,
}

impl ViewResult {
    /// Deserialize an instance of type `T` from bytes of JSON text sourced from the
    /// execution result of this call. This conversion can fail if the structure of
    /// the internal state does not meet up with [`serde::de::DeserializeOwned`]'s
    /// requirements.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.result)?)
    }

    /// Deserialize an instance of type `T` from bytes sourced from this view call's
    /// result. This conversion can fail if the structure of the internal state does
    /// not meet up with [`borsh::BorshDeserialize`]'s requirements.
    pub fn borsh<T: borsh::BorshDeserialize>(&self) -> Result<T> {
        Ok(borsh::BorshDeserialize::try_from_slice(&self.result)?)
    }
}

impl From<CallResult> for ViewResult {
    fn from(result: CallResult) -> Self {
        Self {
            result: result.result,
            logs: result.logs,
        }
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
    /// [`AccessKey`]: near_primitives::account::AccessKey
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
    /// [`AccessKey`]: near_primitives::account::AccessKey
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_result_json() {
        let result = ViewResult {
            result: serde_json::to_vec(&42u32).unwrap(),
            logs: vec![],
        };
        assert_eq!(result.json::<u32>().unwrap(), 42);
    }

    #[test]
    fn view_result_borsh() {
        let result = ViewResult {
            result: borsh::to_vec(&42u32).unwrap(),
            logs: vec![],
        };
        assert_eq!(result.borsh::<u32>().unwrap(), 42);
    }

    /// A minimal, offline `ProcessQuery` used to exercise the combinators without
    /// issuing any RPC requests.
    struct FakeQuery {
        value: i32,
        build_error: Option<&'static str>,
        process_error: Option<&'static str>,
    }

    impl FakeQuery {
        fn ok(value: i32) -> Self {
            Self {
                value,
                build_error: None,
                process_error: None,
            }
        }
    }

    impl ProcessQuery for FakeQuery {
        type Method = methods::query::RpcQueryRequest;
        type Output = i32;

        fn build_request(&self, block_reference: BlockReference) -> Result<Self::Method> {
            if let Some(message) = self.build_error {
                return Err(Error::InvalidArgs(message));
            }
            Ok(Self::Method {
                block_reference,
                request: QueryRequest::ViewAccount {
                    account_id: "alice.near".parse().unwrap(),
                },
            })
        }

        fn process_response(&self, _resp: RpcQueryResponse) -> Result<Self::Output> {
            match self.process_error {
                Some(message) => Err(Error::InvalidArgs(message)),
                None => Ok(self.value),
            }
        }
    }

    fn sample_response() -> RpcQueryResponse {
        RpcQueryResponse {
            kind: QueryResponseKind::CallResult(CallResult {
                result: Vec::new(),
                logs: Vec::new(),
            }),
            block_height: 1,
            block_hash: CryptoHash::default(),
        }
    }

    fn dummy_client() -> Client {
        Client::from_client(
            near_jsonrpc_client::JsonRpcClient::new_client().connect("http://127.0.0.1:1"),
        )
    }

    #[test]
    fn mapped_query_build_request_delegates_to_inner() {
        let client = dummy_client();
        let query = Query::new(&client, FakeQuery::ok(1)).map(|value| value + 1);
        let block_reference = BlockReference::Finality(Finality::Final);

        let request = query.method.build_request(block_reference.clone()).unwrap();
        assert_eq!(request.block_reference, block_reference);

        let failing = FakeQuery {
            build_error: Some("build failed"),
            ..FakeQuery::ok(1)
        };
        let err = failing.build_request(block_reference).unwrap_err();
        assert!(matches!(err, Error::InvalidArgs("build failed")));
    }

    #[test]
    fn mapped_query_map_transforms_response() {
        let client = dummy_client();
        let query = Query::new(&client, FakeQuery::ok(41)).map(|value| value + 1);

        assert_eq!(
            query.method.process_response(sample_response()).unwrap(),
            42
        );
    }

    #[test]
    fn mapped_query_and_then_propagates_errors() {
        let client = dummy_client();
        let query = Query::new(&client, FakeQuery::ok(2)).and_then(|value| {
            if value > 0 {
                Ok(value * 3)
            } else {
                Err(Error::InvalidArgs("non-positive value"))
            }
        });
        assert_eq!(query.method.process_response(sample_response()).unwrap(), 6);

        // The closure's own error branch must propagate, not just the inner query's error.
        let rejected = Query::new(&client, FakeQuery::ok(-1)).and_then(|value| {
            if value > 0 {
                Ok(value * 3)
            } else {
                Err(Error::InvalidArgs("non-positive value"))
            }
        });
        let err = rejected
            .method
            .process_response(sample_response())
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgs("non-positive value")));

        let failing = Query::new(
            &client,
            FakeQuery {
                process_error: Some("inner failed"),
                ..FakeQuery::ok(2)
            },
        )
        .and_then(|value| Ok(value + 1));
        let err = failing
            .method
            .process_response(sample_response())
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgs("inner failed")));
    }

    #[test]
    fn mapped_query_chained_combinators_compose() {
        let client = dummy_client();
        let query = Query::new(&client, FakeQuery::ok(2))
            .map(|value| value * 3)
            .and_then(|value| {
                if value % 2 == 0 {
                    Ok(value + 1)
                } else {
                    Err(Error::InvalidArgs("odd value"))
                }
            })
            .map(|value| value.to_string());

        assert_eq!(
            query.method.process_response(sample_response()).unwrap(),
            "7"
        );
    }

    use near_crypto::KeyType;
    use near_jsonrpc_primitives::types::query::QueryResponseKind;
    use near_primitives::types::SyncCheckpoint;
    use near_primitives::views::{
        AccessKeyPermissionView, BlockHeaderView, ContractCodeView, GasPriceView,
    };

    fn account(id: &str) -> AccountId {
        id.parse().unwrap()
    }

    fn response(kind: QueryResponseKind) -> RpcQueryResponse {
        RpcQueryResponse {
            kind,
            block_height: 9,
            block_hash: CryptoHash::default(),
        }
    }

    fn account_view() -> AccountView {
        AccountView {
            amount: near_token::NearToken::from_near(1),
            locked: near_token::NearToken::ZERO,
            code_hash: CryptoHash::default(),
            storage_usage: 0,
            storage_paid_at: 0,
            global_contract_hash: None,
            global_contract_account_id: None,
            state: Default::default(),
            bootstrap_nonce: None,
        }
    }

    fn wrong_kind() -> QueryResponseKind {
        QueryResponseKind::ViewAccount(account_view())
    }

    fn invalid_data(err: Result<impl std::fmt::Debug>) {
        assert!(
            matches!(err, Err(Error::RpcReturnedInvalidData(_))),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn build_request_for_every_query_type() {
        let client = dummy_client();
        let block = BlockReference::Finality(Finality::Final);

        let view = client.view(&account("contract.near"), "get_info");
        let method = view.method.build_request(block.clone()).unwrap();
        match method.request {
            QueryRequest::CallFunction {
                account_id,
                method_name,
                ..
            } => {
                assert_eq!(account_id, account("contract.near"));
                assert_eq!(method_name, "get_info");
            }
            other => panic!("unexpected request: {other:?}"),
        }

        // Force a deferred serialization error to exercise the `?` in build_request.
        let deferred = Function {
            name: "get".into(),
            args: Err(crate::ops::DeferredArgsError::Serialization(
                std::sync::Arc::new(serde_json::from_str::<u8>("nope").unwrap_err()),
            )),
            deposit: near_token::NearToken::ZERO,
            gas: near_primitives::types::Gas::from_gas(0),
        };
        let bad_args = ViewFunction {
            account_id: account("contract.near"),
            function: deferred,
        };
        assert!(matches!(
            bad_args.build_request(block.clone()),
            Err(Error::Serialization(_))
        ));

        let code = ViewCode {
            account_id: account("contract.near"),
        };
        assert!(matches!(
            code.build_request(block.clone()).unwrap().request,
            QueryRequest::ViewCode { .. }
        ));

        let view_account = ViewAccount {
            account_id: account("alice.near"),
        };
        assert!(matches!(
            view_account.build_request(block.clone()).unwrap().request,
            QueryRequest::ViewAccount { .. }
        ));

        let block_request = ViewBlock.build_request(block.clone()).unwrap();
        assert_eq!(block_request.block_reference, block);

        let state = ViewState {
            account_id: account("alice.near"),
            prefix: Some(vec![1, 2, 3]),
        };
        match state.build_request(block.clone()).unwrap().request {
            QueryRequest::ViewState {
                account_id,
                prefix,
                after_key,
                limit,
                include_proof,
            } => {
                assert_eq!(account_id, account("alice.near"));
                assert_eq!(prefix, StoreKey::from(vec![1, 2, 3]));
                assert!(after_key.is_none());
                assert!(limit.is_none());
                assert!(!include_proof);
            }
            other => panic!("unexpected request: {other:?}"),
        }

        let key = ViewAccessKey {
            account_id: account("alice.near"),
            public_key: PublicKey::empty(KeyType::ED25519),
        };
        assert!(matches!(
            key.build_request(block.clone()).unwrap().request,
            QueryRequest::ViewAccessKey { .. }
        ));

        let keys = ViewAccessKeyList {
            account_id: account("alice.near"),
        };
        match keys.build_request(block.clone()).unwrap().request {
            QueryRequest::ViewAccessKeyList {
                account_id,
                after_key,
                limit,
            } => {
                assert_eq!(account_id, account("alice.near"));
                assert!(after_key.is_none());
                assert!(limit.is_none());
            }
            other => panic!("unexpected request: {other:?}"),
        }

        let gas = GasPrice;
        let by_height = gas.build_request(BlockId::Height(11).into()).unwrap();
        assert_eq!(by_height.block_id, Some(BlockId::Height(11)));
        let by_default = gas.build_request(block).unwrap();
        assert_eq!(by_default.block_id, None);
        invalid_data(
            gas.build_request(BlockReference::SyncCheckpoint(SyncCheckpoint::Genesis))
                .map(|_| ()),
        );
    }

    #[test]
    fn process_response_for_every_query_type_and_wrong_kind() {
        let call = ViewFunction {
            account_id: account("c.near"),
            function: Function::new("get"),
        };
        let view_result = call
            .process_response(response(QueryResponseKind::CallResult(CallResult {
                result: b"hi".to_vec(),
                logs: vec!["log".into()],
            })))
            .unwrap();
        assert_eq!(view_result.result, b"hi");
        assert_eq!(view_result.logs, vec!["log"]);
        invalid_data(call.process_response(response(wrong_kind())));

        let code = ViewCode {
            account_id: account("c.near"),
        };
        let bytes = code
            .process_response(response(QueryResponseKind::ViewCode(ContractCodeView {
                code: vec![1, 2, 3],
                hash: CryptoHash::default(),
            })))
            .unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
        invalid_data(code.process_response(response(wrong_kind())));

        let view_account = ViewAccount {
            account_id: account("c.near"),
        };
        assert_eq!(
            view_account
                .process_response(response(QueryResponseKind::ViewAccount(account_view())))
                .unwrap()
                .amount,
            near_token::NearToken::from_near(1)
        );
        invalid_data(
            view_account.process_response(response(QueryResponseKind::CallResult(
                CallResult::default(),
            ))),
        );

        // ViewBlock is a pass-through: the response is the block itself.
        let block_view = BlockView {
            author: account("c.near"),
            header: BlockHeaderView::default(),
            chunks: vec![],
        };
        assert_eq!(
            ViewBlock
                .process_response(block_view.clone())
                .unwrap()
                .author,
            block_view.author
        );

        let state = ViewState {
            account_id: account("c.near"),
            prefix: None,
        };
        let state_result = state
            .process_response(response(QueryResponseKind::ViewState(ViewStateResult {
                values: vec![],
                proof: vec![],
                last_key: None,
            })))
            .unwrap();
        assert!(state_result.values.is_empty());
        invalid_data(state.process_response(response(wrong_kind())));

        let key = ViewAccessKey {
            account_id: account("c.near"),
            public_key: PublicKey::empty(KeyType::ED25519),
        };
        assert_eq!(
            key.process_response(response(QueryResponseKind::AccessKey(AccessKeyView {
                nonce: 5,
                permission: AccessKeyPermissionView::FullAccess,
            })))
            .unwrap()
            .nonce,
            5
        );
        invalid_data(key.process_response(response(wrong_kind())));

        let keys = ViewAccessKeyList {
            account_id: account("c.near"),
        };
        let list = keys
            .process_response(response(QueryResponseKind::AccessKeyList(AccessKeyList {
                keys: vec![],
                last_key: None,
            })))
            .unwrap();
        assert!(list.keys.is_empty());
        invalid_data(keys.process_response(response(wrong_kind())));

        let gas = GasPrice;
        assert_eq!(
            gas.process_response(GasPriceView {
                gas_price: near_token::NearToken::from_near(2),
            })
            .unwrap(),
            near_token::NearToken::from_near(2)
        );
    }

    #[test]
    fn view_result_decode_errors_and_conversion() {
        let result = ViewResult {
            result: b"not json".to_vec(),
            logs: vec![],
        };
        assert!(result.json::<u32>().is_err());
        assert!(result.borsh::<u32>().is_err());

        let from_call: ViewResult = CallResult {
            result: vec![9],
            logs: vec!["a".into()],
        }
        .into();
        assert_eq!(from_call.result, vec![9]);
        assert_eq!(from_call.logs, vec!["a"]);
    }

    #[test]
    fn query_builders_apply_block_references_and_arguments() {
        let client = dummy_client();

        let by_height = client.view_block().block_height(7);
        assert_eq!(by_height.block_ref, Some(BlockId::Height(7).into()));
        let by_hash = client.view_block().block_hash(CryptoHash::default());
        assert!(matches!(
            by_hash.block_ref,
            Some(BlockReference::BlockId(BlockId::Hash(_)))
        ));
        let by_finality = client
            .view_account(&account("alice.near"))
            .finality(Finality::Final);
        assert_eq!(by_finality.block_ref, Some(Finality::Final.into()));

        let state = client.view_state(&account("alice.near")).prefix(&[4, 5]);
        assert_eq!(state.method.prefix, Some(vec![4, 5]));

        let view = client
            .view(&account("c.near"), "get")
            .args(vec![1])
            .args_json(serde_json::json!({"a": 1}))
            .args_borsh(42u32);
        assert_eq!(
            view.method.function.args.as_ref().unwrap(),
            &borsh::to_vec(&42u32).unwrap()
        );

        // Chunk builders record the selected reference.
        let chunk = client.view_chunk();
        assert!(chunk.chunk_ref.is_none());
        assert!(
            client
                .view_chunk()
                .chunk_hash(CryptoHash::default())
                .chunk_ref
                .is_some()
        );
        assert!(
            client
                .view_chunk()
                .block_hash_and_shard(CryptoHash::default(), ShardId::from(0))
                .chunk_ref
                .is_some()
        );
        assert!(
            client
                .view_chunk()
                .block_height_and_shard(5, ShardId::from(1))
                .chunk_ref
                .is_some()
        );

        assert_eq!(client.gas_price().client.rpc_addr(), client.rpc_addr());
    }

    fn chunk_result(hash: CryptoHash, shard_id: ShardId) -> serde_json::Value {
        let hash = serde_json::to_value(hash).unwrap();
        let signature = serde_json::to_value(
            near_crypto::Signature::from_parts(KeyType::ED25519, &[0u8; 64]).unwrap(),
        )
        .unwrap();
        serde_json::json!({
            "author": "alice.near",
            "header": {
                "chunk_hash": hash,
                "prev_block_hash": hash,
                "outcome_root": hash,
                "prev_state_root": hash,
                "encoded_merkle_root": hash,
                "encoded_length": 0,
                "height_created": 1,
                "height_included": 1,
                "shard_id": shard_id,
                "gas_used": "0",
                "gas_limit": "1000000000000",
                "rent_paid": "0",
                "validator_reward": "0",
                "balance_burnt": "0",
                "outgoing_receipts_root": hash,
                "tx_root": hash,
                "validator_proposals": [],
                "congestion_info": null,
                "bandwidth_requests": null,
                "signature": signature,
            },
            "transactions": [],
            "receipts": [],
        })
    }

    async fn chunk_server(
        block_result: serde_json::Value,
        chunk_result: serde_json::Value,
    ) -> (
        mockito::ServerGuard,
        mockito::Mock,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        let mut server = mockito::Server::new_async().await;
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&bodies);
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                seen.lock().unwrap().push(body.clone());
                let result = match body["method"].as_str() {
                    Some("block") => block_result.clone(),
                    Some("chunk") => chunk_result.clone(),
                    other => panic!("unexpected RPC method: {other:?}"),
                };
                serde_json::json!({"jsonrpc": "2.0", "result": result, "id": 1})
                    .to_string()
                    .into_bytes()
            })
            .expect_at_least(1)
            .create_async()
            .await;
        (server, mock, bodies)
    }

    fn chunk_client(server: &mockito::ServerGuard) -> Client {
        Client::from_client(near_jsonrpc_client::JsonRpcClient::new_client().connect(server.url()))
    }

    #[tokio::test]
    async fn view_chunk_with_explicit_chunk_hash_skips_block_lookup() {
        let chunk_hash = CryptoHash::hash_bytes(b"explicit chunk");
        let (server, mock, bodies) = chunk_server(
            serde_json::Value::Null,
            chunk_result(chunk_hash, ShardId::from(3)),
        )
        .await;
        let client = chunk_client(&server);
        let chunk: ChunkView = client.view_chunk().chunk_hash(chunk_hash).await.unwrap();
        assert_eq!(chunk.header.chunk_hash, chunk_hash);

        {
            let sent = bodies.lock().unwrap();
            let methods: Vec<_> = sent
                .iter()
                .map(|body| body["method"].as_str().unwrap())
                .collect();
            assert_eq!(methods, vec!["chunk"]);
            assert_eq!(
                sent[0]["params"]["chunk_id"],
                serde_json::to_value(chunk_hash).unwrap()
            );
            assert!(sent[0]["params"].get("block_id").is_none());
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn view_chunk_defaults_to_latest_block_and_shard_zero() {
        let block_hash = CryptoHash::hash_bytes(b"latest block");
        let mut block_view = BlockView {
            author: account("alice.near"),
            header: BlockHeaderView::default(),
            chunks: vec![],
        };
        block_view.header.hash = block_hash;
        let chunk_hash = CryptoHash::hash_bytes(b"default chunk");
        let (server, mock, bodies) = chunk_server(
            serde_json::to_value(block_view).unwrap(),
            chunk_result(chunk_hash, ShardId::from(0)),
        )
        .await;
        let client = chunk_client(&server);
        let chunk: ChunkView = client.view_chunk().await.unwrap();
        assert_eq!(chunk.header.chunk_hash, chunk_hash);
        assert_eq!(chunk.header.shard_id, ShardId::from(0));

        {
            let sent = bodies.lock().unwrap();
            let methods: Vec<_> = sent
                .iter()
                .map(|body| body["method"].as_str().unwrap())
                .collect();
            assert_eq!(methods, vec!["block", "chunk"]);
            assert_eq!(
                sent[1]["params"]["block_id"],
                serde_json::to_value(block_hash).unwrap()
            );
            assert_eq!(
                sent[1]["params"]["shard_id"],
                serde_json::to_value(ShardId::from(0)).unwrap()
            );
            assert!(sent[1]["params"].get("chunk_id").is_none());
        }
        mock.assert_async().await;
    }
}
