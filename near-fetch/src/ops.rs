//! All operation types that are generated/used when commiting transactions to the network.

use std::fmt;
use std::task::Poll;
use std::time::Duration;

use near_account_id::AccountId;
use near_crypto::PublicKey;
use near_gas::NearGas;
use near_jsonrpc_client::errors::{JsonRpcError, JsonRpcServerError};
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::account::AccessKey;
use near_primitives::borsh;
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{
    Action, AddKeyAction, CreateAccountAction, DeleteAccountAction, DeleteKeyAction,
    DeployContractAction, FunctionCallAction, StakeAction, TransferAction,
};
use near_primitives::views::FinalExecutionOutcomeView;
use near_token::NearToken;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;

use crate::query::BoxFuture;
use crate::result::ExecutionFinalResult;
use crate::signer::SignerExt;
use crate::{Client, Error, Result};

/// Maximum amount of gas that can be used in a single transaction.
pub const MAX_GAS: NearGas = NearGas::from_tgas(300);

/// Default amount of gas to be used when calling into a function on a contract.
/// This is set to 10 TGas as a default for convenience.
pub const DEFAULT_CALL_FN_GAS: NearGas = NearGas::from_tgas(10);

/// Default amount of deposit to be used when calling into a function on a contract.
/// This is set to 0 NEAR as a default for convenience. Note, that some contracts
/// will require 1 yoctoNEAR to be deposited in order to perform a function.
pub const DEFAULT_CALL_DEPOSIT: NearToken = NearToken::from_near(0);

/// A set of arguments we can provide to a transaction, containing
/// the function name, arguments, the amount of gas to use and deposit.
#[derive(Debug)]
pub struct Function {
    pub(crate) name: String,
    pub(crate) args: Result<Vec<u8>>,
    pub(crate) deposit: NearToken,
    pub(crate) gas: NearGas,
}

impl Function {
    /// Initialize a new instance of [`Function`], tied to a specific function on a
    /// contract that lives directly on a contract we've specified in [`Transaction`].
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            args: Ok(vec![]),
            deposit: DEFAULT_CALL_DEPOSIT,
            gas: DEFAULT_CALL_FN_GAS,
        }
    }

    /// Provide the arguments for the call. These args are serialized bytes from either
    /// a JSON or Borsh serializable set of arguments. To use the more specific versions
    /// with better quality of life, use `args_json` or `args_borsh`.
    pub fn args(mut self, args: Vec<u8>) -> Self {
        if self.args.is_err() {
            return self;
        }
        self.args = Ok(args);
        self
    }

    /// Similar to `args`, specify an argument that is JSON serializable and can be
    /// accepted by the equivalent contract. Recommend to use something like
    /// `serde_json::json!` macro to easily serialize the arguments.
    pub fn args_json<U: serde::Serialize>(mut self, args: U) -> Self {
        match serde_json::to_vec(&args) {
            Ok(args) => self.args = Ok(args),
            Err(e) => self.args = Err(Error::Serialization(e)),
        }
        self
    }

    /// Similar to `args`, specify an argument that is borsh serializable and can be
    /// accepted by the equivalent contract.
    pub fn args_borsh<U: borsh::BorshSerialize>(mut self, args: U) -> Self {
        match args.try_to_vec() {
            Ok(args) => self.args = Ok(args),
            Err(e) => self.args = Err(Error::Io(e)),
        }
        self
    }

    /// Specify the amount of tokens to be deposited where `deposit` is the amount of
    /// tokens in yocto near.
    pub fn deposit(mut self, deposit: NearToken) -> Self {
        self.deposit = deposit;
        self
    }

    /// Specify the amount of gas to be used.
    pub fn gas(mut self, gas: NearGas) -> Self {
        self.gas = gas;
        self
    }

    /// Use the maximum amount of gas possible to perform this function call into the contract.
    pub fn max_gas(self) -> Self {
        self.gas(MAX_GAS)
    }

    pub(crate) fn into_action(self) -> Result<FunctionCallAction> {
        Ok(FunctionCallAction {
            args: self.args?,
            method_name: self.name,
            gas: self.gas.as_gas(),
            deposit: self.deposit.as_yoctonear(),
        })
    }
}

pub struct FunctionCallTransaction<'a, 'b> {
    pub(crate) client: &'a Client,
    pub(crate) signer: &'b dyn SignerExt,
    pub(crate) receiver_id: AccountId,
    pub(crate) function: Function,
    pub(crate) retry_strategy: Option<Box<dyn Iterator<Item = Duration> + Send + Sync>>,
}

impl FunctionCallTransaction<'_, '_> {
    /// Provide the arguments for the call. These args are serialized bytes from either
    /// a JSON or Borsh serializable set of arguments. To use the more specific versions
    /// with better quality of life, use `args_json` or `args_borsh`.
    pub fn args(mut self, args: Vec<u8>) -> Self {
        self.function = self.function.args(args);
        self
    }

    /// Similar to `args`, specify an argument that is JSON serializable and can be
    /// accepted by the equivalent contract. Recommend to use something like
    /// `serde_json::json!` macro to easily serialize the arguments.
    pub fn args_json<U: serde::Serialize>(mut self, args: U) -> Self {
        self.function = self.function.args_json(args);
        self
    }

    /// Similar to `args`, specify an argument that is borsh serializable and can be
    /// accepted by the equivalent contract.
    pub fn args_borsh<U: borsh::BorshSerialize>(mut self, args: U) -> Self {
        self.function = self.function.args_borsh(args);
        self
    }

    /// Specify the amount of tokens to be deposited where `deposit` is the amount of
    /// tokens in yocto near.
    pub fn deposit(mut self, deposit: NearToken) -> Self {
        self.function = self.function.deposit(deposit);
        self
    }

    /// Specify the amount of gas to be used.
    pub fn gas(mut self, gas: NearGas) -> Self {
        self.function = self.function.gas(gas);
        self
    }

    /// Use the maximum amount of gas possible to perform this function call into the contract.
    pub fn max_gas(self) -> Self {
        self.gas(MAX_GAS)
    }
}

impl<'a, 'b> FunctionCallTransaction<'a, 'b> {
    /// Process the transaction, and return the result of the execution.
    pub async fn transact(self) -> Result<ExecutionFinalResult> {
        RetryableTransaction {
            client: self.client.clone(),
            signer: self.signer,
            receiver_id: self.receiver_id,
            actions: self
                .function
                .into_action()
                .map(|action| vec![action.into()]),
            strategy: self.retry_strategy,
        }
        .await
        .map(ExecutionFinalResult::from_view)
    }

    /// Send the transaction to the network to be processed. This will be done asynchronously
    /// without waiting for the transaction to complete. This returns us a [`TransactionStatus`]
    /// for which we can call into [`status`] and/or `.await` to retrieve info about whether
    /// the transaction has been completed or not. Note that `.await` will wait till completion
    /// of the transaction.
    pub async fn transact_async(self) -> Result<AsyncTransactionStatus> {
        let hash = self
            .client
            .send_tx_async(
                self.signer,
                &self.receiver_id,
                vec![self.function.into_action()?.into()],
            )
            .await?;

        Ok(AsyncTransactionStatus::new(
            self.client,
            self.receiver_id,
            hash,
        ))
    }

    /// Retry this transactions if it fails. This will retry the transaction with exponential
    /// backoff. This cannot be used in combination with
    pub fn retry_exponential(self, base_millis: u64, max_retries: usize) -> Self {
        self.retry(
            ExponentialBackoff::from_millis(base_millis)
                .map(jitter)
                .take(max_retries),
        )
    }

    /// Retry this transactions if it fails. This will retry the transaction with the provided
    /// retry strategy.
    pub fn retry(
        mut self,
        strategy: impl Iterator<Item = Duration> + Send + Sync + 'static,
    ) -> Self {
        self.retry_strategy = Some(Box::new(strategy));
        self
    }
}

/// A builder-like object that will allow specifying various actions to be performed
/// in a single transaction. For details on each of the actions, find them in
/// [NEAR transactions](https://docs.near.org/docs/concepts/transaction).
///
/// All actions are performed on the account specified by `receiver_id`.
pub struct Transaction<'a, 'b> {
    client: &'a Client,
    signer: &'b dyn SignerExt,
    receiver_id: AccountId,
    // Result used to defer errors in argument parsing to later when calling into transact
    actions: Result<Vec<Action>>,
    retry_strategy: Option<Box<dyn Iterator<Item = Duration> + Send + Sync>>,
}

impl<'a, 'b> Transaction<'a, 'b> {
    pub(crate) fn new(
        client: &'a Client,
        signer: &'b dyn SignerExt,
        receiver_id: AccountId,
    ) -> Self {
        Self {
            client,
            signer,
            receiver_id,
            actions: Ok(Vec::new()),
            retry_strategy: None,
        }
    }

    /// Process the transaction, and return the result of the execution.
    pub async fn transact(self) -> Result<FinalExecutionOutcomeView> {
        RetryableTransaction {
            client: self.client.clone(),
            signer: self.signer,
            receiver_id: self.receiver_id,
            actions: self.actions,
            strategy: self.retry_strategy,
        }
        .await
    }

    /// Send the transaction to the network to be processed. This will be done asynchronously
    /// without waiting for the transaction to complete.
    pub async fn transact_async(self) -> Result<CryptoHash> {
        self.client
            .send_tx_async(self.signer, &self.receiver_id, self.actions?)
            .await
    }
}

impl Transaction<'_, '_> {
    /// Adds a key to the `receiver_id`'s account, where the public key can be used
    /// later to delete the same key.
    pub fn add_key(mut self, pk: PublicKey, ak: AccessKey) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(
                AddKeyAction {
                    public_key: pk,
                    access_key: ak,
                }
                .into(),
            );
        }

        self
    }

    /// Call into the `receiver_id`'s contract with the specific function arguments.
    pub fn call(mut self, function: Function) -> Self {
        let args = match function.args {
            Ok(args) => args,
            Err(err) => {
                self.actions = Err(err);
                return self;
            }
        };

        if let Ok(actions) = &mut self.actions {
            actions.push(Action::FunctionCall(FunctionCallAction {
                method_name: function.name.to_string(),
                args,
                deposit: function.deposit.as_yoctonear(),
                gas: function.gas.as_gas(),
            }));
        }

        self
    }

    /// Create a new account with the account id being `receiver_id`.
    pub fn create_account(mut self) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(CreateAccountAction {}.into());
        }
        self
    }

    /// Deletes the `receiver_id`'s account. The beneficiary specified by
    /// `beneficiary_id` will receive the funds of the account deleted.
    pub fn delete_account(mut self, beneficiary_id: &AccountId) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(
                DeleteAccountAction {
                    beneficiary_id: beneficiary_id.clone(),
                }
                .into(),
            );
        }
        self
    }

    /// Deletes a key from the `receiver_id`'s account, where the public key is
    /// associated with the access key to be deleted.
    pub fn delete_key(mut self, pk: PublicKey) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(DeleteKeyAction { public_key: pk }.into());
        }
        self
    }

    /// Deploy contract code or WASM bytes to the `receiver_id`'s account.
    pub fn deploy(mut self, code: &[u8]) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(DeployContractAction { code: code.into() }.into());
        }
        self
    }

    /// An action which stakes the signer's tokens and setups a validator public key.
    pub fn stake(mut self, stake: NearToken, pk: PublicKey) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(
                StakeAction {
                    stake: stake.as_yoctonear(),
                    public_key: pk,
                }
                .into(),
            );
        }
        self
    }

    /// Transfer `deposit` amount from `signer`'s account into `receiver_id`'s account.
    pub fn transfer(mut self, deposit: NearToken) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(
                TransferAction {
                    deposit: deposit.as_yoctonear(),
                }
                .into(),
            );
        }
        self
    }

    /// Retry this transactions if it fails. This will retry the transaction with exponential
    /// backoff.
    pub fn retry_exponential(self, base_millis: u64, max_retries: usize) -> Self {
        self.retry(
            ExponentialBackoff::from_millis(base_millis)
                .map(jitter)
                .take(max_retries),
        )
    }

    /// Retry this transactions if it fails. This will retry the transaction with the provided
    /// retry strategy.
    pub fn retry(
        mut self,
        strategy: impl Iterator<Item = Duration> + Send + Sync + 'static,
    ) -> Self {
        self.retry_strategy = Some(Box::new(strategy));
        self
    }
}

pub struct RetryableTransaction<'a> {
    pub(crate) client: Client,
    pub(crate) signer: &'a dyn SignerExt,
    pub(crate) receiver_id: AccountId,
    pub(crate) actions: Result<Vec<Action>>,
    pub(crate) strategy: Option<Box<dyn Iterator<Item = Duration> + Send + Sync>>,
}

impl RetryableTransaction<'_> {
    /// Retry this transactions if it fails. This will retry the transaction with exponential
    /// backoff.
    pub fn retry_exponential(self, base_millis: u64, max_retries: usize) -> Self {
        self.retry(
            ExponentialBackoff::from_millis(base_millis)
                .map(jitter)
                .take(max_retries),
        )
    }

    /// Retry this transactions if it fails. This will retry the transaction with the provided
    /// retry strategy.
    pub fn retry(
        mut self,
        strategy: impl Iterator<Item = Duration> + Send + Sync + 'static,
    ) -> Self {
        self.strategy = Some(Box::new(strategy));
        self
    }
}

impl<'a> std::future::IntoFuture for RetryableTransaction<'a> {
    type Output = Result<FinalExecutionOutcomeView>;
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let actions = self.actions?;
            let action = || async {
                self.client
                    .send_tx_once(self.signer, &self.receiver_id, actions.clone())
                    .await
            };

            if let Some(strategy) = self.strategy {
                Retry::spawn(strategy, action).await
            } else {
                action().await
            }
        })
    }
}

impl Client {
    /// Start calling into a contract on a specific function. Returns a [`FunctionCallTransaction`]
    /// object where we can use to add more parameters such as the arguments, deposit, and gas.
    pub fn call<'b>(
        &self,
        signer: &'b dyn SignerExt,
        contract_id: &AccountId,
        function: &str,
    ) -> FunctionCallTransaction<'_, 'b> {
        FunctionCallTransaction {
            client: self,
            signer,
            receiver_id: contract_id.clone(),
            function: Function::new(function),
            retry_strategy: None,
        }
    }

    /// Start a batch transaction. Returns a [`Transaction`] object that we can
    /// use to add Actions to the batched transaction. Call `transact` to send
    /// the batched transaction to the network.
    pub fn batch<'b>(
        &self,
        signer: &'b dyn SignerExt,
        contract_id: &AccountId,
        function: &str,
    ) -> Transaction<'_, 'b> {
        Transaction::new(self, signer, contract_id.clone()).call(Function::new(function))
    }
}

/// `TransactionStatus` object relating to an [`asynchronous transaction`] on the network.
/// Used to query into the status of the Transaction for whether it has completed or not.
///
/// [`asynchronous transaction`]: https://docs.near.org/api/rpc/transactions#send-transaction-async
#[derive(Clone)]
#[must_use]
pub struct AsyncTransactionStatus {
    client: Client,
    sender_id: AccountId,
    hash: CryptoHash,
}

impl AsyncTransactionStatus {
    pub(crate) fn new(client: &Client, sender_id: AccountId, hash: CryptoHash) -> Self {
        Self {
            client: client.clone(),
            sender_id,
            hash,
        }
    }

    /// Query the status of the transaction. This will return a [`TransactionStatus`]
    /// object that we can use to query into the status of the transaction.
    pub async fn status(&self) -> Result<Poll<ExecutionFinalResult>> {
        let result = self
            .client
            .tx_async_status(&self.sender_id, self.hash)
            .await
            .map(ExecutionFinalResult::from_view);

        match result {
            Ok(result) => Ok(Poll::Ready(result)),
            Err(err) => match err {
                Error::RpcTransactionError(JsonRpcError::ServerError(
                    JsonRpcServerError::HandlerError(RpcTransactionError::UnknownTransaction {
                        ..
                    }),
                )) => Ok(Poll::Pending),
                Error::RpcTransactionError(JsonRpcError::ServerError(
                    JsonRpcServerError::HandlerError(RpcTransactionError::TimeoutError),
                )) => Ok(Poll::Pending),
                other => Err(other),
            },
        }
    }

    /// Wait until the completion of the transaction by polling [`AsyncTransactionStatus::status`].
    pub(crate) async fn wait_default(self) -> Result<ExecutionFinalResult> {
        self.wait(Duration::from_millis(300)).await
    }

    /// Wait until the transaction completes with a given time interval. This will poll the
    /// [`AsyncTransactionStatus::status`] every interval until the transaction completes.
    pub async fn wait(self, interval: Duration) -> Result<ExecutionFinalResult> {
        loop {
            match self.status().await? {
                Poll::Ready(val) => break Ok(val),
                Poll::Pending => (),
            }

            tokio::time::sleep(interval).await;
        }
    }

    /// Get the [`AccountId`] of the account that initiated this transaction.
    pub fn sender_id(&self) -> &AccountId {
        &self.sender_id
    }

    /// Reference [`CryptoHash`] to the submitted transaction, pending completion.
    pub fn hash(&self) -> &CryptoHash {
        &self.hash
    }
}

impl fmt::Debug for AsyncTransactionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransactionStatus")
            .field("sender_id", &self.sender_id)
            .field("hash", &self.hash)
            .finish()
    }
}

impl std::future::IntoFuture for AsyncTransactionStatus {
    type Output = Result<ExecutionFinalResult>;
    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async { self.wait_default().await })
    }
}
