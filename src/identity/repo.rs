//! Persistence for the local node's Ed25519 identity and handle.

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::Database;
use crate::primitives::Timestamp;

use super::model::LocalKeypair;
use super::IdentityError;

/// Persisted local identity record.
#[derive(Debug)]
pub struct LocalIdentity {
    pub handle: String,
    pub keypair: LocalKeypair,
    pub created_at: Timestamp,
}

/// Runs the identity domain's SQLite schema migration.
pub fn migrate(conn: &Connection) -> Result<(), IdentityError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS local_identity (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            handle TEXT NOT NULL,
            secret_seed BLOB NOT NULL,
            created_at_ms INTEGER NOT NULL
        );",
    )
    .map_err(IdentityError::Storage)
}

#[derive(Clone)]
pub struct IdentityRepo {
    db: Database,
}

impl IdentityRepo {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn load(&self) -> Result<Option<LocalIdentity>, IdentityError> {
        self.db
            .exec(|conn| {
                let row = conn
                    .query_row(
                        "SELECT handle, secret_seed, created_at_ms FROM local_identity WHERE id = 1",
                        [],
                        |r| {
                            let handle: String = r.get(0)?;
                            let seed_blob: Vec<u8> = r.get(1)?;
                            let created_at_ms: i64 = r.get(2)?;
                            Ok((handle, seed_blob, created_at_ms))
                        },
                    )
                    .optional()
                    .map_err(IdentityError::Storage)?;

                match row {
                    None => Ok(None),
                    Some((handle, seed_blob, created_at_ms)) => {
                        let seed: [u8; 32] = seed_blob
                            .try_into()
                            .map_err(|_| IdentityError::CorruptKeyMaterial)?;
                        Ok(Some(LocalIdentity {
                            handle,
                            keypair: LocalKeypair::from_seed(seed),
                            created_at: Timestamp::from_millis(created_at_ms),
                        }))
                    }
                }
            })
            .await
            .map_err(IdentityError::Database)?
    }

    pub async fn save(&self, identity: &LocalIdentity) -> Result<(), IdentityError> {
        let handle = identity.handle.clone();
        let seed = *identity.keypair.expose_secret_seed();
        let created_at_ms = identity.created_at.as_millis();

        self.db
            .exec(move |conn| {
                conn.execute(
                    "INSERT INTO local_identity (id, handle, secret_seed, created_at_ms)
                     VALUES (1, ?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET
                        handle = excluded.handle,
                        secret_seed = excluded.secret_seed,
                        created_at_ms = excluded.created_at_ms",
                    params![handle, seed.as_slice(), created_at_ms],
                )
                .map_err(IdentityError::Storage)?;
                Ok(())
            })
            .await
            .map_err(IdentityError::Database)?
    }
}
