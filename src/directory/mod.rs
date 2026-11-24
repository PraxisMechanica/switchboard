//! Directory vertical slice: signed peer cards, Git phone book, SQLite cache, CLI.

pub mod card;
pub mod cli;
pub mod conformance;
pub mod git_phonebook;
pub mod repo;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::db::{Database, DbError};
use crate::identity::{self, IdentityError, LocalKeypair, PeerId, Signed};

pub use card::{verify_peer_card, AdvertisedExtension, PeerCard};
pub use cli::DirectoryCmd;
pub use git_phonebook::GitPhonebook;
pub use repo::{migrate, DirectoryRepo};

#[derive(Debug, Error)]
pub enum DirectoryError {
    #[error("identity error: {0}")]
    Identity(#[from] IdentityError),

    #[error("peer card signer {signer} does not match card peer_id {card_peer_id}")]
    SignerMismatch {
        signer: PeerId,
        card_peer_id: PeerId,
    },

    #[error("git command `{command}` failed: {details}")]
    Git { command: String, details: String },

    #[error("directory I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("directory serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("directory storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("database runtime error: {0}")]
    Database(#[from] DbError),
}

/// Port abstraction for publishing and fetching signed peer cards.
#[async_trait::async_trait]
pub trait DirectoryPort: Send + Sync {
    async fn publish(&self, card: Signed<PeerCard>) -> Result<(), DirectoryError>;

    async fn fetch_all(&self) -> Result<Vec<Signed<PeerCard>>, DirectoryError>;
}

/// Deterministic in-memory [`DirectoryPort`] for tests.
#[derive(Debug, Default, Clone)]
pub struct MemoryDirectory {
    cards: Arc<Mutex<BTreeMap<String, Signed<PeerCard>>>>,
}

impl MemoryDirectory {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl DirectoryPort for MemoryDirectory {
    async fn publish(&self, card: Signed<PeerCard>) -> Result<(), DirectoryError> {
        let mut guard = self
            .cards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.insert(card.payload.handle.clone(), card);
        Ok(())
    }

    async fn fetch_all(&self) -> Result<Vec<Signed<PeerCard>>, DirectoryError> {
        let guard = self
            .cards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard.values().cloned().collect())
    }
}

/// Summary returned by [`DirectoryService::sync`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub synced: usize,
    pub rejected: usize,
}

/// Application service coordinating peer card publication, verification, and caching.
#[derive(Clone)]
pub struct DirectoryService<D: DirectoryPort> {
    repo: DirectoryRepo,
    port: D,
}

impl<D: DirectoryPort> DirectoryService<D> {
    pub fn new(db: Database, port: D) -> Self {
        Self {
            repo: DirectoryRepo::new(db),
            port,
        }
    }

    pub fn repo(&self) -> &DirectoryRepo {
        &self.repo
    }

    pub fn port(&self) -> &D {
        &self.port
    }

    pub async fn publish_local(
        &self,
        keypair: &LocalKeypair,
        card: PeerCard,
    ) -> Result<(), DirectoryError> {
        if card.peer_id != keypair.peer_id() {
            return Err(DirectoryError::SignerMismatch {
                signer: keypair.peer_id(),
                card_peer_id: card.peer_id,
            });
        }

        identity::validate_handle(&card.handle)?;

        let signed = identity::sign(keypair, card);
        let verified = verify_peer_card(signed.clone())?;

        self.repo.upsert_verified(&verified).await?;
        self.port.publish(signed).await?;

        Ok(())
    }

    pub async fn sync(&self) -> Result<SyncReport, DirectoryError> {
        let cards = self.port.fetch_all().await?;
        let mut synced = 0usize;
        let mut rejected = 0usize;

        for signed in cards {
            let handle = signed.payload.handle.clone();
            match verify_peer_card(signed) {
                Ok(verified) => {
                    self.repo.upsert_verified(&verified).await?;
                    synced += 1;
                }
                Err(err) => {
                    tracing::warn!(
                        handle = %handle,
                        error = %err,
                        "rejected invalid peer card during directory sync"
                    );
                    rejected += 1;
                }
            }
        }

        Ok(SyncReport { synced, rejected })
    }

    pub async fn list_cached(&self) -> Result<Vec<PeerCard>, DirectoryError> {
        self.repo.list().await
    }

    pub async fn resolve_handle(&self, handle: &str) -> Result<Option<PeerCard>, DirectoryError> {
        self.repo.find_by_handle(handle).await
    }

    pub async fn resolve_peer_id(
        &self,
        peer_id: PeerId,
    ) -> Result<Option<PeerCard>, DirectoryError> {
        self.repo.find_by_peer_id(peer_id).await
    }
}
