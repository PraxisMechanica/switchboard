//! Identity vertical slice: node Ed25519 keys, generic `Signed<T>`/`Verified<T>`, persistence, CLI.

pub mod cli;
pub mod model;
pub mod repo;

use thiserror::Error;

use crate::db::{Database, DbError};
use crate::primitives::Clock;

pub use model::{sign, verify, CanonicalBytes, LocalKeypair, PeerId, Signature, Signed, Verified};
pub use repo::{migrate, IdentityRepo, LocalIdentity};

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("invalid peer id")]
    InvalidPeerId,

    #[error("invalid handle '{0}': must be 1..=32 lowercase ASCII alphanumeric, '-', or '_'")]
    InvalidHandle(String),

    #[error("signature verification failed")]
    InvalidSignature,

    #[error("node identity is not initialized; run `switchboard init --name <handle>`")]
    NotInitialized,

    #[error("node identity already initialized as '{0}' (pass --force to overwrite)")]
    AlreadyInitialized(String),

    #[error("corrupt stored key material")]
    CorruptKeyMaterial,

    #[error("identity storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("database runtime error: {0}")]
    Database(#[from] DbError),
}

/// Validates a human node handle (`[a-z0-9_-]{1,32}`).
pub fn validate_handle(raw: &str) -> Result<String, IdentityError> {
    if raw.is_empty()
        || raw.len() > 32
        || !raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(IdentityError::InvalidHandle(raw.to_string()));
    }
    Ok(raw.to_string())
}

/// Application service owning local node identity lifecycle.
#[derive(Clone)]
pub struct IdentityService<C: Clock> {
    repo: IdentityRepo,
    clock: C,
}

impl<C: Clock> IdentityService<C> {
    pub fn new(db: Database, clock: C) -> Self {
        Self {
            repo: IdentityRepo::new(db),
            clock,
        }
    }

    pub async fn init(&self, handle: &str, force: bool) -> Result<LocalIdentity, IdentityError> {
        let valid_handle = validate_handle(handle)?;
        if !force {
            if let Some(existing) = self.repo.load().await? {
                return Err(IdentityError::AlreadyInitialized(existing.handle));
            }
        }
        let identity = LocalIdentity {
            handle: valid_handle,
            keypair: LocalKeypair::generate(),
            created_at: self.clock.now(),
        };
        self.repo.save(&identity).await?;
        Ok(identity)
    }

    pub async fn load_required(&self) -> Result<LocalIdentity, IdentityError> {
        self.repo.load().await?.ok_or(IdentityError::NotInitialized)
    }

    pub async fn load_optional(&self) -> Result<Option<LocalIdentity>, IdentityError> {
        self.repo.load().await
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crate::primitives::FixedClock;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MsgA(Vec<u8>);

    impl CanonicalBytes for MsgA {
        const DOMAIN_TAG: &'static [u8] = b"switchboard.test.msg_a.v1";

        fn canonical_bytes(&self) -> Vec<u8> {
            self.0.clone()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MsgB(Vec<u8>);

    impl CanonicalBytes for MsgB {
        const DOMAIN_TAG: &'static [u8] = b"switchboard.test.msg_b.v1";

        fn canonical_bytes(&self) -> Vec<u8> {
            self.0.clone()
        }
    }

    proptest! {
        #[test]
        fn sign_and_verify_roundtrips_for_arbitrary_payloads(bytes in any::<Vec<u8>>()) {
            let kp = LocalKeypair::generate();
            let signed = sign(&kp, MsgA(bytes.clone()));
            let verified = verify(signed).expect("valid signature");
            prop_assert_eq!(verified.signer(), kp.peer_id());
            prop_assert_eq!(verified.payload().0.as_slice(), bytes.as_slice());
        }
    }

    #[test]
    fn domain_tag_prevents_cross_type_replay() {
        let kp = LocalKeypair::generate();
        let signed_a = sign(&kp, MsgA(b"same-bytes".to_vec()));
        let forged_b = Signed {
            payload: MsgB(b"same-bytes".to_vec()),
            signer: signed_a.signer,
            signature: signed_a.signature,
        };
        assert!(matches!(
            verify(forged_b),
            Err(IdentityError::InvalidSignature)
        ));
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let kp = LocalKeypair::generate();
        let mut signed = sign(&kp, MsgA(b"original".to_vec()));
        signed.payload = MsgA(b"tampered".to_vec());
        assert!(matches!(
            verify(signed),
            Err(IdentityError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn identity_init_and_reload_roundtrips() {
        let db = Database::open_in_memory().unwrap();
        db.with_conn_sync(|c| migrate(c).unwrap());
        let svc = IdentityService::new(db, FixedClock::new(1_700_000_000_000));

        let created = svc.init("alice", false).await.unwrap();
        let reloaded = svc.load_required().await.unwrap();
        assert_eq!(created.keypair.peer_id(), reloaded.keypair.peer_id());
        assert_eq!(reloaded.handle, "alice");

        assert!(matches!(
            svc.init("bob", false).await,
            Err(IdentityError::AlreadyInitialized(_))
        ));

        let replaced = svc.init("bob", true).await.unwrap();
        assert_eq!(replaced.handle, "bob");
        assert_ne!(replaced.keypair.peer_id(), created.keypair.peer_id());
    }
}
