use near_account_id::AccountId;
use near_crypto::Signer;
use near_gas::NearGas;
use near_primitives::borsh;
use near_primitives::transaction::FunctionCallAction;
use near_primitives::views::FinalExecutionOutcomeView;
use near_token::NearToken;

use crate::query::BoxFuture;
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

pub struct CallableFunction<'a, S> {
    pub(crate) client: &'a Client,
    pub(crate) signer: S,
    pub(crate) contract_id: AccountId,
    pub(crate) function: Function,
}

impl<S> CallableFunction<'_, S> {
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

impl Client {
    /// Start calling into a contract on a specific function.
    pub fn call<S: Signer + ExposeAccountId>(
        &self,
        signer: S,
        contract_id: &AccountId,
        function: &str,
    ) -> CallableFunction<'_, S> {
        CallableFunction {
            client: self,
            signer,
            contract_id: contract_id.clone(),
            function: Function::new(function),
        }
    }
}

impl<'a, S: Signer + ExposeAccountId + 'static> std::future::IntoFuture
    for CallableFunction<'a, S>
{
    type Output = Result<FinalExecutionOutcomeView>;
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            self.client
                .send_tx(
                    &self.signer,
                    &self.contract_id,
                    vec![self.function.into_action()?.into()],
                )
                .await
        })
    }
}
