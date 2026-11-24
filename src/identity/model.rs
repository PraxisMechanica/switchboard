//! Cryptographic node identity types and generic proof-carrying signature wrappers.

use std::fmt;
use std::str::FromStr;

use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use rand::rngs::OsRng;
use secrecy::{ExposeSecret, SecretBox};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::IdentityError;

/// Canonical 32-byte Ed25519 public key identifying a switchboard node.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId([u8; 32]);

impl PeerId {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, IdentityError> {
        VerifyingKey::from_bytes(&bytes).map_err(|_| IdentityError::InvalidPeerId)?;
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn short(self) -> String {
        self.to_hex()[..12].to_string()
    }

    pub(crate) fn verifying_key(&self) -> Result<VerifyingKey, IdentityError> {
        VerifyingKey::from_bytes(&self.0).map_err(|_| IdentityError::InvalidPeerId)
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({})", self.short())
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for PeerId {
    type Err = IdentityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s.trim()).map_err(|_| IdentityError::InvalidPeerId)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| IdentityError::InvalidPeerId)?;
        Self::from_bytes(arr)
    }
}

impl Serialize for PeerId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for PeerId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// 64-byte Ed25519 signature serialized as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; 64]);

impl Signature {
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({}..)", &self.to_hex()[..12])
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        let arr: [u8; 64] = bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("signature must be 64 bytes"))?;
        Ok(Self(arr))
    }
}

/// Local Ed25519 keypair holding secret seed material inside [`SecretBox`].
pub struct LocalKeypair {
    seed: SecretBox<[u8; 32]>,
    peer_id: PeerId,
}

impl fmt::Debug for LocalKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalKeypair")
            .field("peer_id", &self.peer_id)
            .finish_non_exhaustive()
    }
}

impl LocalKeypair {
    pub fn generate() -> Self {
        let signing = SigningKey::generate(&mut OsRng);
        Self::from_seed(signing.to_bytes())
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&seed);
        let peer_id = PeerId(signing.verifying_key().to_bytes());
        Self {
            seed: SecretBox::new(Box::new(seed)),
            peer_id,
        }
    }

    pub fn clone_keypair(&self) -> Self {
        Self::from_seed(*self.seed.expose_secret())
    }

    pub const fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Exposes the raw 32-byte Ed25519 seed for persistence or transport binding.
    pub fn expose_secret_seed(&self) -> &[u8; 32] {
        self.seed.expose_secret()
    }

    pub(crate) fn sign_digest(&self, digest: &[u8; 32]) -> Signature {
        let signing = SigningKey::from_bytes(self.seed.expose_secret());
        let sig = signing.sign(digest);
        Signature(sig.to_bytes())
    }
}

/// Canonical byte serialization contract for signed domain payloads.
pub trait CanonicalBytes {
    /// Unique domain-separation tag preventing cross-type signature replay.
    const DOMAIN_TAG: &'static [u8];

    fn canonical_bytes(&self) -> Vec<u8>;
}

/// Wire representation of a signed payload prior to verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed<T> {
    pub payload: T,
    pub signer: PeerId,
    pub signature: Signature,
}

/// Proof-carrying wrapper minted exclusively by [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified<T> {
    payload: T,
    signer: PeerId,
}

impl<T> Verified<T> {
    pub const fn payload(&self) -> &T {
        &self.payload
    }

    pub const fn signer(&self) -> PeerId {
        self.signer
    }

    pub fn into_parts(self) -> (PeerId, T) {
        (self.signer, self.payload)
    }
}

fn domain_digest<T: CanonicalBytes>(payload: &T) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(T::DOMAIN_TAG);
    hasher.update([0u8]);
    hasher.update(payload.canonical_bytes());
    let out = hasher.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    buf
}

/// Signs any [`CanonicalBytes`] domain value with the local keypair.
pub fn sign<T: CanonicalBytes>(keypair: &LocalKeypair, payload: T) -> Signed<T> {
    let digest = domain_digest(&payload);
    let signature = keypair.sign_digest(&digest);
    Signed {
        payload,
        signer: keypair.peer_id(),
        signature,
    }
}

/// Verifies a [`Signed<T>`] envelope and mints a proof-carrying [`Verified<T>`].
pub fn verify<T: CanonicalBytes>(signed: Signed<T>) -> Result<Verified<T>, IdentityError> {
    let digest = domain_digest(&signed.payload);
    let vk = signed.signer.verifying_key()?;
    let sig = ed25519_dalek::Signature::from_bytes(signed.signature.as_bytes());
    vk.verify(&digest, &sig)
        .map_err(|_| IdentityError::InvalidSignature)?;
    Ok(Verified {
        payload: signed.payload,
        signer: signed.signer,
    })
}
