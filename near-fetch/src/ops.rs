//! All operation types that are generated/used when committing transactions to the network.

use std::fmt;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use near_account_id::AccountId;
use near_crypto::PublicKey;
use near_jsonrpc_client::errors::{JsonRpcError, JsonRpcServerError};
use near_jsonrpc_primitives::types::transactions::RpcTransactionError;
use near_primitives::account::AccessKey;
use near_primitives::borsh;
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{
    Action, AddKeyAction, CreateAccountAction, DeleteAccountAction, DeleteKeyAction,
    DeployContractAction, FunctionCallAction, StakeAction, TransferAction,
};
use near_primitives::types::Gas;
use near_primitives::views::{FinalExecutionOutcomeView, TxExecutionStatus};
use near_token::NearToken;
use tokio_retry::Retry;
use tokio_retry::strategy::{ExponentialBackoff, jitter};

use crate::query::BoxFuture;
use crate::result::ExecutionFinalResult;
use crate::signer::SignerExt;
use crate::{Client, Error, Result};

/// Maximum amount of gas that can be used in a single transaction.
pub const MAX_GAS: Gas = Gas::from_teragas(300);

/// Default amount of gas to be used when calling into a function on a contract.
/// This is set to 10 TGas as a default for convenience.
pub const DEFAULT_CALL_FN_GAS: Gas = Gas::from_teragas(10);

/// Default amount of deposit to be used when calling into a function on a contract.
/// This is set to 0 NEAR as a default for convenience. Note, that some contracts
/// will require 1 yoctoNEAR to be deposited in order to perform a function.
pub const DEFAULT_CALL_DEPOSIT: NearToken = NearToken::from_near(0);

/// Deferred form of the two serialization errors a [`Function`]'s args can hold.
///
/// [`Error`] is not cloneable, so the source error is kept behind an [`Arc`] and
/// converted back into the original variant when the args are consumed. Only the
/// message survives the round trip: the reconstructed error is equivalent in
/// variant and message, but is not the original error value.
#[derive(Debug, Clone)]
pub(crate) enum DeferredArgsError {
    Serialization(Arc<serde_json::Error>),
    Io(Arc<std::io::Error>),
}

impl From<DeferredArgsError> for Error {
    fn from(err: DeferredArgsError) -> Self {
        match err {
            DeferredArgsError::Serialization(err) => Error::Serialization(
                <serde_json::Error as serde::de::Error>::custom(err.to_string()),
            ),
            DeferredArgsError::Io(err) => Error::Io(std::io::Error::other(err.to_string())),
        }
    }
}

/// A set of arguments we can provide to a transaction, containing
/// the function name, arguments, the amount of gas to use and deposit.
#[derive(Debug)]
pub struct Function {
    pub(crate) name: String,
    pub(crate) args: Result<Vec<u8>, DeferredArgsError>,
    pub(crate) deposit: NearToken,
    pub(crate) gas: Gas,
}

impl Clone for Function {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            args: self.args.clone(),
            deposit: self.deposit,
            gas: self.gas,
        }
    }
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
            Err(e) => self.args = Err(DeferredArgsError::Serialization(Arc::new(e))),
        }
        self
    }

    /// Similar to `args`, specify an argument that is borsh serializable and can be
    /// accepted by the equivalent contract.
    pub fn args_borsh<U: borsh::BorshSerialize>(mut self, args: U) -> Self {
        match borsh::to_vec(&args) {
            Ok(args) => self.args = Ok(args),
            Err(e) => self.args = Err(DeferredArgsError::Io(Arc::new(e))),
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
    pub fn gas(mut self, gas: Gas) -> Self {
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
            gas: self.gas,
            deposit: self.deposit,
        })
    }
}

/// Builder for a single function-call transaction, created by [`Client::call`].
///
/// Use the builder methods to set arguments, gas, and deposit before calling
/// [`FunctionCallTransaction::transact`] or [`FunctionCallTransaction::transact_async`].
pub struct FunctionCallTransaction<'a> {
    pub(crate) client: Client,
    pub(crate) signer: &'a dyn SignerExt,
    pub(crate) receiver_id: AccountId,
    pub(crate) function: Function,
    pub(crate) retry_strategy: Option<Box<dyn Iterator<Item = Duration> + Send + Sync>>,
    pub(crate) wait_until: TxExecutionStatus,
}

impl FunctionCallTransaction<'_> {
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
    pub fn gas(mut self, gas: Gas) -> Self {
        self.function = self.function.gas(gas);
        self
    }

    /// Use the maximum amount of gas possible to perform this function call into the contract.
    pub fn max_gas(self) -> Self {
        self.gas(MAX_GAS)
    }
}

impl FunctionCallTransaction<'_> {
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
            wait_until: self.wait_until,
        }
        .await
        .map(ExecutionFinalResult::from_view)
    }

    /// Send the transaction to the network to be processed. This will be done asynchronously
    /// without waiting for the transaction to complete. This returns us an [`AsyncTransactionStatus`]
    /// for which we can call into [`AsyncTransactionStatus::status`] and/or `.await` to retrieve info about whether
    /// the transaction has been completed or not. Note that `.await` will wait till completion
    /// of the transaction.
    pub async fn transact_async(self) -> Result<AsyncTransactionStatus> {
        let sender_id = self.signer.account_id().clone();
        let hash = self
            .client
            .send_tx_async(
                self.signer,
                &self.receiver_id,
                vec![self.function.into_action()?.into()],
            )
            .await?;

        Ok(AsyncTransactionStatus::new(self.client, sender_id, hash))
    }

    /// Retry this transaction if it fails, using exponential backoff with jitter.
    ///
    /// `base_millis` is the initial delay and `max_retries` caps the number of
    /// attempts after the first one. This replaces any retry strategy previously
    /// set with [`FunctionCallTransaction::retry`].
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

    /// Specifies the status to wait until the transaction reaches in the network. The default
    /// value is [`TxExecutionStatus::ExecutedOptimistic`] if not specified by this function.
    pub fn wait_until(mut self, wait_until: TxExecutionStatus) -> Self {
        self.wait_until = wait_until;
        self
    }
}

/// A builder-like object that will allow specifying various actions to be performed
/// in a single transaction. For details on each of the actions, find them in
/// [NEAR transactions](https://docs.near.org/docs/concepts/transaction).
///
/// All actions are performed on the account specified by `receiver_id`.
pub struct Transaction<'a> {
    client: Client,
    signer: &'a dyn SignerExt,
    receiver_id: AccountId,
    // Result used to defer errors in argument parsing to later when calling into transact
    actions: Result<Vec<Action>>,
    retry_strategy: Option<Box<dyn Iterator<Item = Duration> + Send + Sync>>,
    wait_until: TxExecutionStatus,
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(client: &Client, signer: &'a dyn SignerExt, receiver_id: AccountId) -> Self {
        Self {
            client: client.clone(),
            signer,
            receiver_id,
            actions: Ok(Vec::new()),
            retry_strategy: None,
            wait_until: TxExecutionStatus::default(),
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
            wait_until: self.wait_until,
        }
        .await
    }

    /// Send the transaction to the network to be processed. This will be done asynchronously
    /// without waiting for the transaction to complete.
    pub async fn transact_async(self) -> Result<AsyncTransactionStatus> {
        let sender_id = self.signer.account_id().clone();
        let hash = self
            .client
            .send_tx_async(self.signer, &self.receiver_id, self.actions?)
            .await?;

        Ok(AsyncTransactionStatus::new(self.client, sender_id, hash))
    }
}

impl Transaction<'_> {
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
                self.actions = Err(err.into());
                return self;
            }
        };

        if let Ok(actions) = &mut self.actions {
            actions.push(Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: function.name.to_string(),
                args,
                deposit: function.deposit,
                gas: function.gas,
            })));
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
                    stake,
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
            actions.push(TransferAction { deposit }.into());
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

    /// Specifies the status to wait until the transaction reaches in the network. The default
    /// value is [`TxExecutionStatus::ExecutedOptimistic`] if not specified by this function.
    pub fn wait_until(mut self, wait_until: TxExecutionStatus) -> Self {
        self.wait_until = wait_until;
        self
    }
}

/// A future-like transaction builder returned by [`Client::send_tx`].
///
/// Awaiting it sends the transaction once; call [`RetryableTransaction::retry`]
/// or [`RetryableTransaction::retry_exponential`] first to configure retries on
/// failure. It also implements [`std::future::IntoFuture`], so it can be awaited directly.
pub struct RetryableTransaction<'a> {
    pub(crate) client: Client,
    pub(crate) signer: &'a dyn SignerExt,
    pub(crate) receiver_id: AccountId,
    pub(crate) actions: Result<Vec<Action>>,
    pub(crate) strategy: Option<Box<dyn Iterator<Item = Duration> + Send + Sync>>,
    pub(crate) wait_until: TxExecutionStatus,
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

    /// Specifies the status to wait until the transaction reaches in the network. The default
    /// value is [`TxExecutionStatus::ExecutedOptimistic`] if not specified by this function.
    pub fn wait_until(mut self, wait_until: TxExecutionStatus) -> Self {
        self.wait_until = wait_until;
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
                    .send_tx_once(
                        self.signer,
                        &self.receiver_id,
                        actions.clone(),
                        self.wait_until.clone(),
                    )
                    .await
            };

            if let Some(strategy) = self.strategy {
                Retry::start(strategy, action).await
            } else {
                action().await
            }
        })
    }
}

impl Client {
    /// Start calling into a contract on a specific function. Returns a [`FunctionCallTransaction`]
    /// object where we can use to add more parameters such as the arguments, deposit, and gas.
    pub fn call<'a>(
        &self,
        signer: &'a dyn SignerExt,
        contract_id: &AccountId,
        function: &str,
    ) -> FunctionCallTransaction<'a> {
        FunctionCallTransaction {
            client: self.clone(),
            signer,
            receiver_id: contract_id.clone(),
            function: Function::new(function),
            retry_strategy: None,
            wait_until: TxExecutionStatus::default(),
        }
    }

    /// Start a batch transaction. Returns a [`Transaction`] object that we can
    /// use to add Actions to the batched transaction. Call `transact` to send
    /// the batched transaction to the network.
    pub fn batch<'a>(&self, signer: &'a dyn SignerExt, receiver_id: &AccountId) -> Transaction<'a> {
        Transaction::new(self, signer, receiver_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn function_clone_preserves_args() {
        let function = Function::new("hello").args(b"world".to_vec());
        assert_eq!(function.clone().args.unwrap(), b"world");
    }

    #[test]
    fn function_clone_preserves_deferred_args_error_variant() {
        let original = serde_json::from_str::<u8>("not a number").unwrap_err();
        let message = original.to_string();
        let function = Function {
            name: "hello".into(),
            args: Err(DeferredArgsError::Serialization(Arc::new(original))),
            deposit: DEFAULT_CALL_DEPOSIT,
            gas: DEFAULT_CALL_FN_GAS,
        };

        let cloned = function.clone();
        let deferred = cloned.args.unwrap_err();
        assert!(matches!(deferred, DeferredArgsError::Serialization(_)));

        let err: Error = deferred.into();
        assert!(
            matches!(err, Error::Serialization(_)),
            "unexpected error variant: {err:?}"
        );
        assert_eq!(err.to_string(), message);
    }

    use crate::signer::ExposeAccountId;
    use near_crypto::{InMemorySigner, KeyType, SecretKey};
    use near_primitives::account::{AccessKey, AccessKeyPermission};

    struct FailingSerialize;
    impl serde::Serialize for FailingSerialize {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("cannot serialize"))
        }
    }

    struct FailingBorsh;
    impl borsh::BorshSerialize for FailingBorsh {
        fn serialize<W: borsh::io::Write>(&self, _: &mut W) -> borsh::io::Result<()> {
            Err(borsh::io::Error::other("cannot borsh serialize"))
        }
    }

    fn signer(id: &str) -> InMemorySigner {
        let account_id: AccountId = id.parse().unwrap();
        let secret_key = SecretKey::from_random(KeyType::ED25519);
        let public_key = secret_key.public_key();
        InMemorySigner {
            account_id,
            public_key,
            secret_key,
        }
    }

    fn dummy_client() -> Client {
        Client::new("http://127.0.0.1:1")
    }

    #[test]
    fn function_builders_and_into_action() {
        let function = Function::new("hello")
            .args(vec![1, 2])
            .deposit(NearToken::from_near(1))
            .gas(Gas::from_gas(100));
        assert_eq!(function.name, "hello");
        assert_eq!(function.args.unwrap(), vec![1, 2]);
        assert_eq!(function.deposit, NearToken::from_near(1));
        assert_eq!(function.gas, Gas::from_gas(100));

        let json_args = serde_json::json!({"a": 1});
        let json_fn = Function::new("f").args_json(&json_args);
        assert_eq!(
            json_fn.args.unwrap(),
            serde_json::to_vec(&json_args).unwrap()
        );

        let borsh_fn = Function::new("f").args_borsh(42u32);
        assert_eq!(borsh_fn.args.unwrap(), borsh::to_vec(&42u32).unwrap());

        assert_eq!(Function::new("f").max_gas().gas, MAX_GAS);

        let action = Function::new("f").args(vec![9]).into_action().unwrap();
        assert_eq!(action.method_name, "f");
        assert_eq!(action.args, vec![9]);
        assert_eq!(action.gas, DEFAULT_CALL_FN_GAS);
        assert_eq!(action.deposit, DEFAULT_CALL_DEPOSIT);

        let bad_json = Function::new("f").args_json(FailingSerialize);
        assert!(matches!(
            bad_json.args,
            Err(DeferredArgsError::Serialization(_))
        ));
        assert!(
            Function::new("f")
                .args_json(FailingSerialize)
                .into_action()
                .is_err()
        );

        let bad_borsh = Function::new("f").args_borsh(FailingBorsh);
        assert!(matches!(bad_borsh.args, Err(DeferredArgsError::Io(_))));

        // An error is retained when a later `args` call is made.
        assert!(
            Function::new("f")
                .args_json(FailingSerialize)
                .args(vec![1])
                .into_action()
                .is_err()
        );
    }

    #[test]
    fn deferred_args_error_converts_to_io_variant() {
        let err: Error = DeferredArgsError::Io(Arc::new(std::io::Error::other("boom"))).into();
        assert!(matches!(err, Error::Io(_)));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn function_call_transaction_builders() {
        let client = dummy_client();
        let signer = signer("alice.near");
        let receiver: AccountId = "contract.near".parse().unwrap();

        let tx = client
            .call(&signer, &receiver, "f")
            .args(vec![1])
            .args_json(serde_json::json!({"a": 1}))
            .args_borsh(42u32)
            .deposit(NearToken::from_near(1))
            .gas(Gas::from_gas(5))
            .max_gas()
            .retry_exponential(10, 3)
            .wait_until(TxExecutionStatus::Final);
        assert_eq!(tx.function.name, "f");
        assert_eq!(tx.function.args.unwrap(), borsh::to_vec(&42u32).unwrap());
        assert_eq!(tx.function.deposit, NearToken::from_near(1));
        assert_eq!(tx.function.gas, MAX_GAS);
        assert!(tx.retry_strategy.is_some());
        assert_eq!(tx.wait_until, TxExecutionStatus::Final);
        assert_eq!(tx.receiver_id, receiver);

        let with_retry = client
            .call(&signer, &receiver, "f")
            .retry(std::iter::once(Duration::from_millis(1)));
        assert!(with_retry.retry_strategy.is_some());
    }

    #[test]
    fn transaction_action_builders() {
        let client = dummy_client();
        let signer = signer("alice.near");
        let receiver: AccountId = "bob.near".parse().unwrap();
        let pk = PublicKey::empty(KeyType::ED25519);
        let access_key = AccessKey {
            nonce: 1,
            permission: AccessKeyPermission::FullAccess,
        };

        let tx = client
            .batch(&signer, &receiver)
            .add_key(pk.clone(), access_key)
            .call(Function::new("f").args(vec![1]))
            .create_account()
            .delete_account(&"beneficiary.near".parse().unwrap())
            .delete_key(pk.clone())
            .deploy(&[1, 2, 3])
            .stake(NearToken::from_near(1), pk.clone())
            .transfer(NearToken::from_near(2))
            .retry_exponential(10, 3)
            .wait_until(TxExecutionStatus::Final);
        assert_eq!(tx.actions.unwrap().len(), 8);
        assert!(tx.retry_strategy.is_some());
        assert_eq!(tx.wait_until, TxExecutionStatus::Final);
        assert_eq!(tx.receiver_id, receiver);

        let with_retry = client
            .batch(&signer, &receiver)
            .retry(std::iter::once(Duration::from_millis(1)));
        assert!(with_retry.retry_strategy.is_some());

        // Deferred argument errors short-circuit subsequent actions.
        let bad = client
            .batch(&signer, &receiver)
            .call(Function::new("f").args_json(FailingSerialize))
            .transfer(NearToken::from_near(1));
        assert!(matches!(bad.actions, Err(Error::Serialization(_))));
    }

    #[test]
    fn retryable_transaction_builders() {
        let client = dummy_client();
        let signer = signer("alice.near");
        let receiver: AccountId = "bob.near".parse().unwrap();

        let tx = client
            .send_tx(&signer, &receiver, vec![])
            .retry_exponential(10, 3)
            .wait_until(TxExecutionStatus::Final);
        assert!(tx.strategy.is_some());
        assert_eq!(tx.wait_until, TxExecutionStatus::Final);
        assert_eq!(tx.receiver_id, receiver);

        let tx = client
            .send_tx(&signer, &receiver, vec![])
            .retry(std::iter::once(Duration::from_millis(1)));
        assert!(tx.strategy.is_some());

        let tx = client
            .send_tx(&signer, &receiver, vec![])
            .wait_until(TxExecutionStatus::Final);
        assert!(tx.strategy.is_none());
        assert_eq!(tx.wait_until, TxExecutionStatus::Final);
    }

    use near_crypto::Signature;
    use near_jsonrpc_client::JsonRpcClient;
    use near_jsonrpc_primitives::types::transactions::RpcTransactionResponse;
    use near_primitives::errors::{InvalidTxError, TxExecutionError};
    use near_primitives::types::Gas;
    use near_primitives::views::{
        BlockHeaderView, BlockView, ExecutionOutcomeView, ExecutionOutcomeWithIdView,
        ExecutionStatusView, FinalExecutionOutcomeView, FinalExecutionOutcomeViewEnum,
        FinalExecutionStatus,
    };

    fn jsonrpc_ok(result: serde_json::Value) -> Vec<u8> {
        serde_json::json!({"jsonrpc": "2.0", "result": result, "id": 1})
            .to_string()
            .into_bytes()
    }

    fn tx_result(status: FinalExecutionStatus, finality: TxExecutionStatus) -> serde_json::Value {
        let outcome = FinalExecutionOutcomeView {
            status,
            transaction: near_primitives::views::SignedTransactionView {
                signer_id: "alice.near".parse().unwrap(),
                public_key: PublicKey::empty(KeyType::ED25519),
                nonce: 1,
                receiver_id: "bob.near".parse().unwrap(),
                actions: vec![],
                _priority_fee: 0,
                signature: Signature::from_parts(KeyType::ED25519, &[0u8; 64]).unwrap(),
                hash: CryptoHash::default(),
                nonce_index: None,
                nonce_mode: None,
            },
            transaction_outcome: ExecutionOutcomeWithIdView {
                proof: vec![],
                block_hash: CryptoHash::default(),
                id: CryptoHash::default(),
                outcome: ExecutionOutcomeView {
                    logs: vec![],
                    receipt_ids: vec![],
                    gas_burnt: Gas::from_gas(1),
                    tokens_burnt: NearToken::ZERO,
                    executor_id: "alice.near".parse().unwrap(),
                    status: ExecutionStatusView::SuccessValue(b"ok".to_vec()),
                    metadata: Default::default(),
                },
            },
            receipts_outcome: vec![],
        };
        serde_json::to_value(RpcTransactionResponse {
            final_execution_outcome: Some(FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
                outcome,
            )),
            final_execution_status: finality,
        })
        .unwrap()
    }

    type CapturedBodies = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

    async fn rpc_server(
        tx_result: serde_json::Value,
    ) -> (mockito::ServerGuard, mockito::Mock, CapturedBodies) {
        let mut server = mockito::Server::new_async().await;
        let block = serde_json::to_value(BlockView {
            author: "alice.near".parse().unwrap(),
            header: BlockHeaderView::default(),
            chunks: vec![],
        })
        .unwrap();
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().unwrap()).unwrap();
                seen.lock().unwrap().push(body.clone());
                match body["method"].as_str() {
                    Some("query") => jsonrpc_ok(serde_json::json!({
                        "nonce": 5,
                        "permission": "FullAccess",
                        "block_height": 10,
                        "block_hash": "11111111111111111111111111111111",
                    })),
                    Some("block") => jsonrpc_ok(block.clone()),
                    _ => jsonrpc_ok(tx_result.clone()),
                }
            })
            .expect_at_least(1)
            .create_async()
            .await;
        (server, mock, bodies)
    }

    /// The wire format for `tx` flattens `TransactionInfo`, so the sender is in
    /// `params.sender_account_id` (not nested under a `transaction_info` key).
    fn captured_tx_senders(bodies: &[serde_json::Value]) -> Vec<String> {
        bodies
            .iter()
            .filter(|body| body["method"] == "tx")
            .map(|body| {
                body["params"]["sender_account_id"]
                    .as_str()
                    .expect("tx request must carry sender_account_id")
                    .to_string()
            })
            .collect()
    }

    async fn server_client(server: &mockito::ServerGuard) -> Client {
        Client::from_client(JsonRpcClient::new_client().connect(server.url()))
    }

    #[tokio::test]
    async fn retryable_transaction_into_future_sends_once_and_retries() {
        let success = tx_result(
            FinalExecutionStatus::SuccessValue(b"ok".to_vec()),
            TxExecutionStatus::Final,
        );
        let (server, _, _) = rpc_server(success).await;
        let client = server_client(&server).await;
        let signer = signer("alice.near");
        let receiver: AccountId = "bob.near".parse().unwrap();

        let outcome = client.send_tx(&signer, &receiver, vec![]).await.unwrap();
        assert_eq!(
            outcome.status,
            FinalExecutionStatus::SuccessValue(b"ok".to_vec())
        );

        // The retry branch uses `Retry::start` with the configured strategy.
        let outcome = client
            .send_tx(&signer, &receiver, vec![])
            .retry(std::iter::once(Duration::ZERO))
            .await
            .unwrap();
        assert_eq!(
            outcome.status,
            FinalExecutionStatus::SuccessValue(b"ok".to_vec())
        );
    }

    #[tokio::test]
    async fn function_call_transaction_transacts_sync_and_async() {
        let success = tx_result(
            FinalExecutionStatus::SuccessValue(serde_json::to_vec(&7u32).unwrap()),
            TxExecutionStatus::Final,
        );
        let (server, _, bodies) = rpc_server(success).await;
        let client = server_client(&server).await;
        let signer = signer("alice.near");
        let receiver: AccountId = "contract.near".parse().unwrap();

        let result = client
            .call(&signer, &receiver, "get")
            .args_json(serde_json::json!({"key": "value"}))
            .gas(Gas::from_gas(100))
            .deposit(NearToken::from_near(1))
            .transact()
            .await
            .unwrap();
        assert_eq!(result.json::<u32>().unwrap(), 7);

        let status = client
            .call(&signer, &receiver, "get")
            .transact_async()
            .await
            .unwrap();
        // The status handle must track the signer (transaction sender), not the
        // receiver: outcome lookups and shard routing use the sender account id.
        assert_eq!(status.sender_id(), signer.account_id());
        assert_ne!(status.sender_id(), &receiver);
        assert_ne!(status.hash(), &CryptoHash::default());
        assert!(format!("{status:?}").contains("TransactionStatus"));
        assert!(matches!(status.status().await.unwrap(), Poll::Ready(_)));

        // Assert what actually went over the wire: status lookups must use the
        // signer account, never the contract being called.
        let senders = captured_tx_senders(&bodies.lock().unwrap());
        assert_eq!(senders, vec![signer.account_id().to_string()]);
        assert!(senders.iter().all(|id| id != receiver.as_str()));
    }

    #[tokio::test]
    async fn batch_transaction_transacts_sync_and_async() {
        let success = tx_result(
            FinalExecutionStatus::SuccessValue(b"ok".to_vec()),
            TxExecutionStatus::Final,
        );
        let (server, _, bodies) = rpc_server(success).await;
        let client = server_client(&server).await;
        let signer = signer("alice.near");
        let receiver: AccountId = "bob.near".parse().unwrap();

        let outcome = client
            .batch(&signer, &receiver)
            .transfer(NearToken::from_near(1))
            .transact()
            .await
            .unwrap();
        assert!(matches!(
            outcome.status,
            FinalExecutionStatus::SuccessValue(_)
        ));

        let status = client
            .batch(&signer, &receiver)
            .transfer(NearToken::from_near(1))
            .transact_async()
            .await
            .unwrap();
        // This is the second `sender_id` call site (`Transaction::transact_async`).
        // Reverting it must fail this test even when the getter is read back.
        assert_eq!(status.sender_id(), signer.account_id());
        assert_ne!(status.sender_id(), &receiver);
        assert!(matches!(status.status().await.unwrap(), Poll::Ready(_)));
        assert!(status.clone().into_future().await.is_ok());
        assert!(
            status
                .clone()
                .wait_until(TxExecutionStatus::Final)
                .await
                .is_ok()
        );
        assert!(status.wait(Duration::ZERO).await.is_ok());

        let senders = captured_tx_senders(&bodies.lock().unwrap());
        assert_eq!(senders.len(), 4, "all four status lookups must be recorded");
        assert!(senders.iter().all(|id| id == signer.account_id().as_str()));
        assert!(senders.iter().all(|id| id != receiver.as_str()));
    }

    #[tokio::test]
    async fn deferred_argument_errors_short_circuit_async_transactions() {
        let success = tx_result(
            FinalExecutionStatus::SuccessValue(b"ok".to_vec()),
            TxExecutionStatus::Final,
        );
        let (server, _, _) = rpc_server(success).await;
        let client = server_client(&server).await;
        let signer = signer("alice.near");
        let receiver: AccountId = "bob.near".parse().unwrap();

        assert!(
            client
                .call(&signer, &receiver, "get")
                .args_json(FailingSerialize)
                .transact()
                .await
                .is_err()
        );
        assert!(
            client
                .call(&signer, &receiver, "get")
                .args_json(FailingSerialize)
                .transact_async()
                .await
                .is_err()
        );
        assert!(
            client
                .batch(&signer, &receiver)
                .call(Function::new("get").args_json(FailingSerialize))
                .transact_async()
                .await
                .is_err()
        );
        assert!(
            client
                .send_tx(&signer, &receiver, vec![])
                .retry(std::iter::once(Duration::ZERO))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn status_maps_pending_and_error_responses() {
        let error_body = |err: RpcTransactionError| {
            let error: near_jsonrpc_primitives::errors::RpcError = err.into();
            serde_json::json!({"jsonrpc": "2.0", "error": error, "id": 1}).to_string()
        };

        let pending_cases = [
            error_body(RpcTransactionError::UnknownTransaction {
                requested_transaction_hash: CryptoHash::default(),
            }),
            error_body(RpcTransactionError::TimeoutError(None)),
        ];
        for body in pending_cases {
            let mut server = mockito::Server::new_async().await;
            let mock = server
                .mock("POST", "/")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(body)
                // UnknownTransaction and TimeoutError are retryable status errors.
                .expect(crate::RETRIES_PER_ENDPOINT)
                .create_async()
                .await;
            let client = server_client(&server).await;
            let status = AsyncTransactionStatus::new(
                client,
                "alice.near".parse().unwrap(),
                CryptoHash::default(),
            );
            assert!(matches!(status.status().await.unwrap(), Poll::Pending));
            mock.assert_async().await;
        }

        // A non-final response reported by the node is also pending.
        let mut server = mockito::Server::new_async().await;
        let pending = serde_json::to_value(RpcTransactionResponse {
            final_execution_outcome: None,
            final_execution_status: TxExecutionStatus::Included,
        })
        .unwrap();
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(jsonrpc_ok(pending))
            .expect(1)
            .create_async()
            .await;
        let status = AsyncTransactionStatus::new(
            server_client(&server).await,
            "alice.near".parse().unwrap(),
            CryptoHash::default(),
        );
        assert!(matches!(status.status().await.unwrap(), Poll::Pending));
        mock.assert_async().await;

        // Other transaction errors are surfaced to the caller.
        let mut server = mockito::Server::new_async().await;
        let body = error_body(RpcTransactionError::InvalidTransaction {
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
        let status = AsyncTransactionStatus::new(
            server_client(&server).await,
            "alice.near".parse().unwrap(),
            CryptoHash::default(),
        );
        assert!(matches!(
            status.status().await,
            Err(Error::RpcTransactionError(_))
        ));
        mock.assert_async().await;

        // A failed execution is surfaced as `Error::TxExecution` carrying the
        // exact error reported by the node, not a freshly built placeholder.
        let tx_error = TxExecutionError::InvalidTxError(InvalidTxError::InvalidNonce {
            tx_nonce: 1,
            ak_nonce: 2,
        });
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(jsonrpc_ok(tx_result(
                FinalExecutionStatus::Failure(tx_error.clone()),
                TxExecutionStatus::Final,
            )))
            .expect(1)
            .create_async()
            .await;
        let status = AsyncTransactionStatus::new(
            server_client(&server).await,
            "alice.near".parse().unwrap(),
            CryptoHash::default(),
        );
        let Poll::Ready(result) = status.status().await.unwrap() else {
            panic!("expected a terminal status");
        };
        assert!(result.is_failure());
        match result.into_result() {
            Err(Error::TxExecution(reported)) => assert_eq!(*reported, tx_error),
            other => panic!("unexpected status result: {other:?}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn wait_polls_again_after_retryable_pending_status() {
        let success = tx_result(
            FinalExecutionStatus::SuccessValue(b"ok".to_vec()),
            TxExecutionStatus::Final,
        );
        let rpc_error: near_jsonrpc_primitives::errors::RpcError =
            RpcTransactionError::UnknownTransaction {
                requested_transaction_hash: CryptoHash::default(),
            }
            .into();
        let pending = serde_json::json!({"jsonrpc": "2.0", "error": rpc_error, "id": 1})
            .to_string()
            .into_bytes();

        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |_| {
                // The first `status()` call exhausts the retry budget on the
                // retryable pending error; the following poll succeeds.
                if counter.fetch_add(1, Ordering::SeqCst) < crate::RETRIES_PER_ENDPOINT {
                    pending.clone()
                } else {
                    jsonrpc_ok(success.clone())
                }
            })
            .expect(crate::RETRIES_PER_ENDPOINT + 1)
            .create_async()
            .await;
        let status = AsyncTransactionStatus::new(
            server_client(&server).await,
            "alice.near".parse().unwrap(),
            CryptoHash::default(),
        );

        let result = status.wait(Duration::ZERO).await.unwrap();
        assert!(result.is_success());
        assert_eq!(result.raw_bytes().unwrap(), b"ok");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            crate::RETRIES_PER_ENDPOINT + 1,
            "wait() must poll more than once before returning Ready"
        );
        mock.assert_async().await;
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
    pub(crate) fn new(client: Client, sender_id: AccountId, hash: CryptoHash) -> Self {
        Self {
            client,
            sender_id,
            hash,
        }
    }

    /// Poll the status of the transaction.
    ///
    /// Returns `Ok(Poll::Pending)` while the transaction has not been included yet,
    /// and `Ok(Poll::Ready(result))` once its execution result is available.
    pub async fn status(&self) -> Result<Poll<ExecutionFinalResult>> {
        let result = self
            .client
            .status_tx_async(&self.sender_id, self.hash, TxExecutionStatus::Included)
            .await
            .map(ExecutionFinalResult::from_view);

        match result {
            Ok(result) => Ok(Poll::Ready(result)),
            Err(err) => match err {
                Error::RpcTransactionError(err) => match &*err {
                    JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
                        RpcTransactionError::UnknownTransaction { .. },
                    )) => Ok(Poll::Pending),

                    JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
                        RpcTransactionError::TimeoutError(_),
                    )) => Ok(Poll::Pending),
                    _ => Err(Error::RpcTransactionError(err)),
                },
                Error::RpcTransactionPending => Ok(Poll::Pending),
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

    /// Waits until a specific transaction status is reached.
    pub async fn wait_until(self, wait_until: TxExecutionStatus) -> Result<ExecutionFinalResult> {
        self.client
            .status_tx_async(&self.sender_id, self.hash, wait_until)
            .await
            .map(ExecutionFinalResult::from_view)
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
