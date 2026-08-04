use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use near_account_id::AccountId;
use near_crypto::{vrf, InMemorySigner, PublicKey, SecretKey, Signature, Signer};
use near_primitives::borsh;
use near_primitives::hash::CryptoHash;
use near_primitives::views::AccessKeyPermissionView;

use crate::error::{Error, Result};
use crate::Client;

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

    pub fn public_key(&self) -> PublicKey {
        self.current_signer().public_key()
    }

    pub fn sign(&self, data: &[u8]) -> Signature {
        self.fetch_and_rotate_signer().sign(data)
    }

    pub fn compute_vrf_with_proof(&self, data: &[u8]) -> (vrf::Value, vrf::Proof) {
        self.current_signer().compute_vrf_with_proof(data)
    }
}

/// A trait for exposing the account id of any object.
pub trait ExposeAccountId {
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
    fn sign(&self, data: &[u8]) -> Signature;
    fn public_key(&self) -> PublicKey;

    /// Signs an [NEP-413](https://github.com/near/NEPs/blob/master/neps/nep-0413.md) off-chain
    /// message, widely used for authentication and proof of account ownership. This is
    /// the same as signing the message's hash: computing the hash is infallible, but
    /// signing itself may panic (for example, [`Signer::Empty`]).
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

        let hash = payload.compute_hash().unwrap();
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
        let hash = payload.compute_hash().unwrap();
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
}
