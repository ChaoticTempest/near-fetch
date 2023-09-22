use std::sync::atomic::{AtomicUsize, Ordering};

use near_account_id::AccountId;
use near_crypto::{vrf, InMemorySigner, PublicKey, SecretKey, Signature, Signer};

use crate::error::{Error, Result};

/// A key rotating in memory signer, that will rotate the key used for signing on
/// each call to [`Signer::sign`].
pub struct KeyRotatingSigner {
    signers: Vec<InMemorySigner>,
    counter: AtomicUsize,
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

    pub fn from_signers(iterable: impl IntoIterator<Item = InMemorySigner>) -> Self {
        Self {
            signers: iterable.into_iter().collect(),
            counter: AtomicUsize::new(0),
        }
    }

    fn current_signer(&self) -> &InMemorySigner {
        &self.signers[self.counter.load(Ordering::SeqCst) % self.signers.len()]
    }

    // TODO: implement key rotation strategy injection?
    /// Fetches the current signer and rotates to the next one.
    fn fetch_and_rotate_signer(&self) -> &InMemorySigner {
        // note: overflow will just wrap on atomics:
        let idx = self.counter.fetch_add(1, Ordering::SeqCst);
        &self.signers[idx % self.signers.len()]
    }
}

impl Signer for KeyRotatingSigner {
    fn sign(&self, data: &[u8]) -> Signature {
        self.fetch_and_rotate_signer().sign(data)
    }

    fn public_key(&self) -> PublicKey {
        self.current_signer().public_key()
    }

    fn compute_vrf_with_proof(&self, data: &[u8]) -> (vrf::Value, vrf::Proof) {
        self.current_signer().compute_vrf_with_proof(data)
    }
}
