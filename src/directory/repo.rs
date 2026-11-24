//! SQLite persistence for verified peer cards.

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::Database;
use crate::identity::{PeerId, Verified};

use super::card::PeerCard;
use super::DirectoryError;

/// Runs the directory domain's SQLite schema migration.
pub fn migrate(conn: &Connection) -> Result<(), DirectoryError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS directory_cards (
            peer_id TEXT PRIMARY KEY,
            handle TEXT NOT NULL UNIQUE,
            card_json TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );",
    )
    .map_err(DirectoryError::Storage)
}

/// Repository storing cryptographically verified peer cards in SQLite.
#[derive(Clone)]
pub struct DirectoryRepo {
    db: Database,
}

impl DirectoryRepo {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Inserts or updates a verified peer card, requiring a [`Verified<PeerCard>`] proof.
    pub async fn upsert_verified(&self, card: &Verified<PeerCard>) -> Result<(), DirectoryError> {
        let payload = card.payload();
        let peer_id = payload.peer_id.to_hex();
        let handle = payload.handle.clone();
        let card_json = serde_json::to_string(payload)?;
        let updated_at_ms = payload.updated_at.as_millis();

        self.db
            .exec(move |conn| {
                let tx = conn.transaction().map_err(DirectoryError::Storage)?;
                tx.execute(
                    "DELETE FROM directory_cards WHERE handle = ?1 AND peer_id != ?2",
                    params![handle, peer_id],
                )
                .map_err(DirectoryError::Storage)?;
                tx.execute(
                    "INSERT INTO directory_cards (peer_id, handle, card_json, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(peer_id) DO UPDATE SET
                        handle = excluded.handle,
                        card_json = excluded.card_json,
                        updated_at_ms = excluded.updated_at_ms",
                    params![peer_id, handle, card_json, updated_at_ms],
                )
                .map_err(DirectoryError::Storage)?;
                tx.commit().map_err(DirectoryError::Storage)?;
                Ok(())
            })
            .await
            .map_err(DirectoryError::Database)?
    }

    pub async fn list(&self) -> Result<Vec<PeerCard>, DirectoryError> {
        self.db
            .exec(|conn| {
                let mut stmt = conn
                    .prepare("SELECT card_json FROM directory_cards ORDER BY handle ASC")
                    .map_err(DirectoryError::Storage)?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(DirectoryError::Storage)?;

                let mut cards = Vec::new();
                for row in rows {
                    let json = row.map_err(DirectoryError::Storage)?;
                    let card: PeerCard = serde_json::from_str(&json)?;
                    cards.push(card);
                }
                Ok(cards)
            })
            .await
            .map_err(DirectoryError::Database)?
    }

    pub async fn find_by_handle(&self, handle: &str) -> Result<Option<PeerCard>, DirectoryError> {
        let handle = handle.to_string();

        self.db
            .exec(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT card_json FROM directory_cards WHERE handle = ?1",
                        params![handle],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DirectoryError::Storage)?;

                row.map(|json| serde_json::from_str::<PeerCard>(&json))
                    .transpose()
                    .map_err(DirectoryError::Serialization)
            })
            .await
            .map_err(DirectoryError::Database)?
    }

    pub async fn find_by_peer_id(
        &self,
        peer_id: PeerId,
    ) -> Result<Option<PeerCard>, DirectoryError> {
        let peer_id_hex = peer_id.to_hex();

        self.db
            .exec(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT card_json FROM directory_cards WHERE peer_id = ?1",
                        params![peer_id_hex],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DirectoryError::Storage)?;

                row.map(|json| serde_json::from_str::<PeerCard>(&json))
                    .transpose()
                    .map_err(DirectoryError::Serialization)
            })
            .await
            .map_err(DirectoryError::Database)?
    }
}
