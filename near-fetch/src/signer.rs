//! Signing helpers, NEP-413 off-chain messages, and key-rotating signers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use near_account_id::AccountId;
use near_crypto::{InMemorySigner, PublicKey, SecretKey, Signature, Signer, vrf};
use near_primitives::borsh;
use near_primitives::hash::CryptoHash;
use near_primitives::views::AccessKeyPermissionView;

use crate::Client;
use crate::error::{Error, Result};

/// An off-chain message to sign, as defined by
/// [NEP-413](https://github.com/near/NEPs/blob/master/neps/nep-0413.md). Widely used for
/// authentication and proof of account ownership.
#[derive(
    Debug,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    borsh::BorshSerialize,
    borsh::BorshDeserialize,
)]
pub struct OffchainMessage {
    /// The message that is being transmitted.
    pub message: String,
    /// A nonce that uniquely identifies this instance of the message, denoted as a 32 bytes array.
    pub nonce: [u8; 32],
    /// The recipient to whom the message is destined (e.g. "alice.near" or "myapp.com").
    pub recipient: String,
    /// A callback URL that will be called with the signed message as a query parameter.
    pub callback_url: Option<String>,
}

impl OffchainMessage {
    const MESSAGE_PREFIX: u32 = (1u32 << 31) + 413;

    /// Compute the NEP-413 hash for this message.
    ///
    /// This is infallible: serializing this type into a `Vec` only writes its
    /// `String`, `[u8; 32]`, `String`, and `Option<String>` fields, none of which
    /// can fail to serialize into a growable buffer.
    pub fn compute_hash(&self) -> CryptoHash {
        let mut bytes = Self::MESSAGE_PREFIX.to_le_bytes().to_vec();
        bytes.extend(borsh::to_vec(self).expect("NEP-413 message serialization is infallible"));
        CryptoHash::hash_bytes(&bytes)
    }

    /// Extract timestamp from the nonce (first 8 bytes as big-endian u64 milliseconds).
    pub fn extract_timestamp_from_nonce(&self) -> u64 {
        let mut timestamp = [0u8; 8];
        timestamp.copy_from_slice(&self.nonce[..8]);
        u64::from_be_bytes(timestamp)
    }

    /// Verify a signature over this message, and that the public key belongs to the
    /// account as a full access key, as required by NEP-413.
    pub async fn verify(
        &self,
        account_id: &AccountId,
        public_key: PublicKey,
        signature: &Signature,
        client: &Client,
    ) -> Result<bool> {
        let hash = self.compute_hash();
        if !signature.verify(hash.as_ref(), &public_key) {
            return Ok(false);
        }

        let (access_key, _, _) = client.access_key(account_id, &public_key).await?;
        Ok(access_key.permission == AccessKeyPermissionView::FullAccess)
    }
}

/// A key rotating in memory signer, that will rotate the key used for signing on
/// each call to [`Signer::sign`].
#[derive(Clone)]
pub struct KeyRotatingSigner {
    signers: Arc<Vec<Signer>>,
    counter: Arc<AtomicUsize>,
}

impl KeyRotatingSigner {
    /// Construct a signer from `(AccountId, SecretKey)` pairs.
    ///
    /// Returns [`Error::InvalidArgs`] if the iterator is empty or if the account
    /// ids are not all identical, since a rotating signer only rotates keys for
    /// a single account.
    pub fn try_from_iter(iter: impl IntoIterator<Item = (AccountId, SecretKey)>) -> Result<Self> {
        let (account_ids, secret_keys): (Vec<AccountId>, Vec<SecretKey>) = iter.into_iter().unzip();
        let mut account_ids = account_ids.into_iter();
        let first = account_ids
            .next()
            .ok_or_else(|| Error::InvalidArgs("must have at least one entry"))?;
        if !account_ids.all(|item| item == first) {
            return Err(Error::InvalidArgs(
                "provided account ids are not all the same",
            ));
        }

        Ok(Self::from_signers(secret_keys.into_iter().map(
            |secret_key| InMemorySigner::from_secret_key(first.clone(), secret_key),
        )))
    }

    /// Construct a signer directly from an iterable of [`Signer`]s.
    ///
    /// The signers are rotated through in iteration order, one per call to [`Signer::sign`].
    /// At least one signer must be supplied; otherwise rotation would divide by zero.
    pub fn from_signers(iterable: impl IntoIterator<Item = Signer>) -> Self {
        Self {
            signers: Arc::new(iterable.into_iter().collect()),
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Fetches the current signer in the key rotation.
    pub fn current_signer(&self) -> &Signer {
        &self.signers[self.counter.load(Ordering::SeqCst) % self.signers.len()]
    }

    // TODO: implement key rotation strategy injection?
    /// Fetches the current signer and rotates to the next one.
    pub fn fetch_and_rotate_signer(&self) -> &Signer {
        // note: overflow will just wrap on atomics:
        let idx = self.counter.fetch_add(1, Ordering::SeqCst);
        &self.signers[idx % self.signers.len()]
    }

    /// The public key of the signer currently selected in the rotation.
    pub fn public_key(&self) -> PublicKey {
        self.current_signer().public_key()
    }

    /// Sign `data` with the current signer, then rotate to the next one.
    pub fn sign(&self, data: &[u8]) -> Signature {
        self.fetch_and_rotate_signer().sign(data)
    }

    /// Compute a VRF value and proof over `data` using the current signer.
    ///
    /// Unlike [`Signer::sign`], this does not advance the rotation.
    pub fn compute_vrf_with_proof(&self, data: &[u8]) -> (vrf::Value, vrf::Proof) {
        self.current_signer().compute_vrf_with_proof(data)
    }
}

/// A trait for exposing the account id of any object.
pub trait ExposeAccountId {
    /// The account id this signer signs on behalf of.
    fn account_id(&self) -> &AccountId;
}

impl ExposeAccountId for InMemorySigner {
    fn account_id(&self) -> &AccountId {
        &self.account_id
    }
}

impl ExposeAccountId for Signer {
    fn account_id(&self) -> &AccountId {
        match self {
            Signer::InMemory(signer) => signer.account_id(),
            Signer::Empty(_) => unimplemented!(),
        }
    }
}

impl ExposeAccountId for KeyRotatingSigner {
    fn account_id(&self) -> &AccountId {
        self.current_signer().account_id()
    }
}

/// A trait for extending the [`Signer`] trait with additional functionality.
pub trait SignerExt: ExposeAccountId + Send + Sync {
    /// Sign the given bytes with this signer's private key.
    fn sign(&self, data: &[u8]) -> Signature;

    /// The public key associated with this signer's private key.
    fn public_key(&self) -> PublicKey;

    /// Signs an [NEP-413](https://github.com/near/NEPs/blob/master/neps/nep-0413.md) off-chain
    /// message, widely used for authentication and proof of account ownership. This is
    /// the same as signing the message's hash: computing the hash is infallible, but
    /// signing itself may panic (for example, [`Signer::Empty`]).
    ///
    /// # Example
    ///
    /// ```
    /// use near_account_id::AccountId;
    /// use near_crypto::{InMemorySigner, KeyType, SecretKey};
    /// use near_fetch::signer::{OffchainMessage, SignerExt};
    ///
    /// let account_id: AccountId = "alice.near".parse().unwrap();
    /// let secret_key = SecretKey::from_random(KeyType::ED25519);
    /// let public_key = secret_key.public_key();
    /// let signer = InMemorySigner::from_secret_key(account_id, secret_key);
    ///
    /// let message = OffchainMessage {
    ///     message: "Hello, NEAR!".to_string(),
    ///     nonce: [0u8; 32],
    ///     recipient: "myapp.com".to_string(),
    ///     callback_url: None,
    /// };
    ///
    /// let signature = signer.sign_offchain(&message);
    /// assert!(signature.verify(message.compute_hash().as_ref(), &public_key));
    /// ```
    fn sign_offchain(&self, message: &OffchainMessage) -> Signature {
        let hash = message.compute_hash();
        self.sign(hash.as_ref())
    }
}

impl SignerExt for InMemorySigner {
    fn sign(&self, data: &[u8]) -> Signature {
        InMemorySigner::sign(self, data)
    }

    fn public_key(&self) -> PublicKey {
        InMemorySigner::public_key(self).clone()
    }
}

impl SignerExt for Signer {
    fn sign(&self, data: &[u8]) -> Signature {
        match self {
            Signer::InMemory(signer) => signer.sign(data),
            Signer::Empty(_) => unimplemented!(),
        }
    }

    fn public_key(&self) -> PublicKey {
        match self {
            Signer::InMemory(signer) => signer.public_key.clone(),
            Signer::Empty(_) => unimplemented!(),
        }
    }
}

impl SignerExt for KeyRotatingSigner {
    fn sign(&self, data: &[u8]) -> Signature {
        KeyRotatingSigner::sign(self, data)
    }

    fn public_key(&self) -> PublicKey {
        KeyRotatingSigner::public_key(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{
        access_key_result_at_height, client_for_server, dummy_client, full_access,
        in_memory_signer, jsonrpc_body,
    };
    use near_crypto::KeyType;

    fn hex_to_hash(hex: &str) -> CryptoHash {
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<_>>();
        CryptoHash(bytes.try_into().unwrap())
    }

    #[test]
    fn compute_hash_matches_nep413_spec_example() {
        // Worked example from the NEP-413 spec, cross-checked against near-api-js:
        // message "hi", nonce [0..=31], recipient "myapp.com", callback "myapp.com/callback".
        let payload = OffchainMessage {
            message: "hi".into(),
            nonce: std::array::from_fn(|i| i as u8),
            recipient: "myapp.com".into(),
            callback_url: Some("myapp.com/callback".into()),
        };

        let hash = payload.compute_hash();
        let expected =
            hex_to_hash("c9ce5de288f7de6d83e2303e56c7615949da83867ea33c0747ed45c47ef50148");
        assert_eq!(hash, expected);
    }

    #[test]
    fn compute_hash_without_callback_url() {
        let payload = OffchainMessage {
            message: "hi".into(),
            nonce: [0u8; 32],
            recipient: "myapp.com".into(),
            callback_url: None,
        };
        let hash = payload.compute_hash();
        let expected =
            hex_to_hash("58909135e0c2d203cce3f7f0ff53d44ee2851fffe37c9d1f569ebcc83f2d5c4c");
        assert_eq!(hash, expected);
    }

    #[test]
    fn extracts_timestamp_from_nonce() {
        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&1_700_000_000_000u64.to_be_bytes());

        let payload = OffchainMessage {
            message: "hi".into(),
            nonce,
            recipient: "myapp.com".into(),
            callback_url: None,
        };
        assert_eq!(payload.extract_timestamp_from_nonce(), 1_700_000_000_000);
    }

    fn sample_message() -> OffchainMessage {
        OffchainMessage {
            message: "hi".into(),
            nonce: [7u8; 32],
            recipient: "myapp.com".into(),
            callback_url: None,
        }
    }

    #[tokio::test]
    async fn verify_rejects_bad_signature_without_rpc() {
        let account_id: AccountId = "alice.near".parse().unwrap();
        let signer = in_memory_signer("alice.near");
        let impostor = in_memory_signer("alice.near");
        let message = sample_message();
        let bad_signature = SignerExt::sign(&impostor, message.compute_hash().as_ref());

        // The client would fail if verify reached the RPC layer; a bad signature
        // must short-circuit to Ok(false) before any access-key lookup.
        let client = dummy_client();
        let result = message
            .verify(
                &account_id,
                SignerExt::public_key(&signer),
                &bad_signature,
                &client,
            )
            .await;
        assert!(matches!(result, Ok(false)));
    }

    #[test]
    fn sign_offchain_signature_verifies_over_hash() {
        let signer = in_memory_signer("alice.near");
        let message = sample_message();
        let signature = SignerExt::sign_offchain(&signer, &message);

        assert!(signature.verify(
            message.compute_hash().as_ref(),
            &SignerExt::public_key(&signer)
        ));
    }

    fn rotating_signer() -> KeyRotatingSigner {
        let first = in_memory_signer("alice.near");
        let second = in_memory_signer("alice.near");
        KeyRotatingSigner::from_signers([Signer::InMemory(first), Signer::InMemory(second)])
    }

    #[test]
    fn try_from_iter_validates_entries() {
        assert!(matches!(
            KeyRotatingSigner::try_from_iter(Vec::new()),
            Err(Error::InvalidArgs("must have at least one entry"))
        ));

        let key = SecretKey::from_random(KeyType::ED25519);
        let mismatched = [
            ("alice.near".parse().unwrap(), key.clone()),
            ("bob.near".parse().unwrap(), key),
        ];
        assert!(matches!(
            KeyRotatingSigner::try_from_iter(mismatched),
            Err(Error::InvalidArgs(
                "provided account ids are not all the same"
            ))
        ));

        let account: AccountId = "alice.near".parse().unwrap();
        let keys = [
            (account.clone(), SecretKey::from_random(KeyType::ED25519)),
            (account.clone(), SecretKey::from_random(KeyType::ED25519)),
        ];
        let rotating = KeyRotatingSigner::try_from_iter(keys).unwrap();
        assert_eq!(rotating.account_id(), &account);
        assert_eq!(rotating.current_signer().account_id(), &account);
    }

    #[test]
    fn key_rotation_advances_on_sign_but_not_on_public_key_or_vrf() {
        let rotating = rotating_signer();
        let first_key = rotating.public_key();

        let signature = SignerExt::sign(&rotating, b"payload");
        assert!(signature.verify(b"payload", &first_key));
        let second_key = rotating.public_key();
        assert_ne!(first_key, second_key);

        // Reading the current signer or public key does not advance the rotation.
        assert_eq!(rotating.public_key(), second_key);
        assert_eq!(rotating.fetch_and_rotate_signer().public_key(), second_key);
        assert_eq!(rotating.public_key(), first_key);

        let signature = KeyRotatingSigner::sign(&rotating, b"next");
        assert!(signature.verify(b"next", &first_key));
        assert_eq!(rotating.public_key(), second_key);

        let (value, proof) = KeyRotatingSigner::compute_vrf_with_proof(&rotating, b"vrf");
        assert_eq!(value.0.len(), 32);
        assert_eq!(proof.0.len(), 64);
        assert_eq!(rotating.public_key(), second_key);
    }

    #[test]
    fn expose_account_id_impls() {
        let signer = in_memory_signer("alice.near");
        assert_eq!(
            signer.account_id(),
            &"alice.near".parse::<AccountId>().unwrap()
        );

        let wrapped = Signer::InMemory(signer);
        assert_eq!(
            wrapped.account_id(),
            &"alice.near".parse::<AccountId>().unwrap()
        );

        let rotating = rotating_signer();
        assert_eq!(
            rotating.account_id(),
            &"alice.near".parse::<AccountId>().unwrap()
        );

        let signature = SignerExt::sign(&wrapped, b"signer bytes");
        assert!(signature.verify(b"signer bytes", &SignerExt::public_key(&wrapped)));
    }

    #[tokio::test]
    async fn verify_accepts_full_access_and_rejects_function_call_permission() {
        let mut full_access_server = mockito::Server::new_async().await;
        let full_mock = full_access_server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(jsonrpc_body(access_key_result_at_height(
                5,
                full_access(),
                7,
            )))
            .expect(1)
            .create_async()
            .await;

        let client = client_for_server(&full_access_server).await;
        let account_id: AccountId = "alice.near".parse().unwrap();
        let signer = in_memory_signer("alice.near");
        let message = sample_message();
        let signature = SignerExt::sign(&signer, message.compute_hash().as_ref());
        assert!(
            message
                .verify(
                    &account_id,
                    SignerExt::public_key(&signer),
                    &signature,
                    &client
                )
                .await
                .unwrap()
        );
        full_mock.assert_async().await;

        let mut restricted = mockito::Server::new_async().await;
        let function_call = serde_json::json!({
            "FunctionCall": {
                "allowance": null,
                "receiver_id": "contract.near",
                "method_names": []
            }
        });
        let restricted_mock = restricted
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(jsonrpc_body(access_key_result_at_height(
                5,
                function_call,
                7,
            )))
            .expect(1)
            .create_async()
            .await;

        let client = client_for_server(&restricted).await;
        assert!(
            !message
                .verify(
                    &account_id,
                    SignerExt::public_key(&signer),
                    &signature,
                    &client
                )
                .await
                .unwrap()
        );
        restricted_mock.assert_async().await;
    }

    #[tokio::test]
    async fn verify_propagates_access_key_lookup_errors() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(400)
            .expect(1)
            .create_async()
            .await;

        let client = client_for_server(&server).await;
        let account_id: AccountId = "alice.near".parse().unwrap();
        let signer = in_memory_signer("alice.near");
        let message = sample_message();
        let signature = SignerExt::sign(&signer, message.compute_hash().as_ref());
        assert!(
            message
                .verify(
                    &account_id,
                    SignerExt::public_key(&signer),
                    &signature,
                    &client
                )
                .await
                .is_err()
        );
        mock.assert_async().await;
    }
}
