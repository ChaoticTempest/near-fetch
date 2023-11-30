use near_account_id::AccountId;
use near_crypto::{PublicKey, Signer};
use near_gas::NearGas;
use near_primitives::account::AccessKey;
use near_primitives::borsh;
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{
    Action, AddKeyAction, CreateAccountAction, DeleteAccountAction, DeleteKeyAction,
    DeployContractAction, FunctionCallAction, StakeAction, TransferAction,
};
use near_primitives::views::FinalExecutionOutcomeView;
use near_token::NearToken;

use crate::signer::ExposeAccountId;
use crate::{Client, Error, Result};

pub const MAX_GAS: NearGas = NearGas::from_tgas(300);
pub const DEFAULT_CALL_FN_GAS: NearGas = NearGas::from_tgas(10);
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

pub struct FunctionCallTransaction<'a, S> {
    pub(crate) client: &'a Client,
    pub(crate) signer: S,
    pub(crate) receiver_id: AccountId,
    pub(crate) function: Function,
}

impl<S> FunctionCallTransaction<'_, S> {
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

impl<'a, S> FunctionCallTransaction<'a, S>
where
    S: Signer + ExposeAccountId + 'static,
{
    /// Process the transaction, and return the result of the execution.
    pub async fn transact(self) -> Result<FinalExecutionOutcomeView> {
        self.client
            .send_tx(
                &self.signer,
                &self.receiver_id,
                vec![self.function.into_action()?.into()],
            )
            .await
    }

    /// Send the transaction to the network to be processed. This will be done asynchronously
    /// without waiting for the transaction to complete. This returns us a [`TransactionStatus`]
    /// for which we can call into [`status`] and/or `.await` to retrieve info about whether
    /// the transaction has been completed or not. Note that `.await` will wait till completion
    /// of the transaction.
    pub async fn transact_async(self) -> Result<CryptoHash> {
        self.client
            .send_tx_async(
                &self.signer,
                &self.receiver_id,
                vec![self.function.into_action()?.into()],
            )
            .await
    }
}

/// A builder-like object that will allow specifying various actions to be performed
/// in a single transaction. For details on each of the actions, find them in
/// [NEAR transactions](https://docs.near.org/docs/concepts/transaction).
///
/// All actions are performed on the account specified by `receiver_id`.
pub struct Transaction<'a, S> {
    client: &'a Client,
    signer: S,
    receiver_id: AccountId,
    // Result used to defer errors in argument parsing to later when calling into transact
    actions: Result<Vec<Action>>,
}

impl<'a, S> Transaction<'a, S>
where
    S: Signer + ExposeAccountId + 'static,
{
    pub(crate) fn new(client: &'a Client, signer: S, receiver_id: AccountId) -> Self {
        Self {
            client,
            signer,
            receiver_id,
            actions: Ok(Vec::new()),
        }
    }

    /// Process the transaction, and return the result of the execution.
    pub async fn transact(self) -> Result<FinalExecutionOutcomeView> {
        self.client
            .send_tx(&self.signer, &self.receiver_id, self.actions?)
            .await
    }

    /// Send the transaction to the network to be processed. This will be done asynchronously
    /// without waiting for the transaction to complete. This returns us a [`TransactionStatus`]
    /// for which we can call into [`status`] and/or `.await` to retrieve info about whether
    /// the transaction has been completed or not. Note that `.await` will wait till completion
    /// of the transaction.
    ///
    /// [`status`]: TransactionStatus::status
    pub async fn transact_async(self) -> Result<CryptoHash> {
        self.client
            .send_tx_async(&self.signer, &self.receiver_id, self.actions?)
            .await
    }
}

impl<S> Transaction<'_, S> {
    /// Adds a key to the `receiver_id`'s account, where the public key can be used
    /// later to delete the same key.
    pub fn add_key(mut self, pk: PublicKey, ak: AccessKey) -> Self {
        if let Ok(actions) = &mut self.actions {
            actions.push(
                AddKeyAction {
                    public_key: pk.into(),
                    access_key: ak.into(),
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
}

impl Client {
    /// Start calling into a contract on a specific function. Returns a [`FunctionCallTransaction`]
    /// object where we can use to add more parameters such as the arguments, deposit, and gas.
    pub fn call<S: Signer + ExposeAccountId>(
        &self,
        signer: S,
        contract_id: &AccountId,
        function: &str,
    ) -> FunctionCallTransaction<'_, S> {
        FunctionCallTransaction {
            client: self,
            signer,
            receiver_id: contract_id.clone(),
            function: Function::new(function),
        }
    }

    /// Start a batch transaction. Returns a [`Transaction`] object that we can
    /// use to add Actions to the batched transaction. Call `transact` to send
    /// the batched transaction to the network.
    pub fn batch<S: Signer + ExposeAccountId + 'static>(
        &self,
        signer: S,
        contract_id: &AccountId,
        function: &str,
    ) -> Transaction<'_, S> {
        Transaction::new(self, signer, contract_id.clone()).call(Function::new(function))
    }
}
