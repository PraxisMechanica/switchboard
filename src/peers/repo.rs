//! SQLite persistence for peer trust relationships.

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::Database;
use crate::identity::PeerId;
use crate::primitives::Timestamp;

use super::state::{Peer, PeerHandle, PeerStatus};
use super::PeerError;

type RawPeerRow = (String, String, String, Option<String>, i64);

/// Runs the peers domain's SQLite schema migration.
pub fn migrate(conn: &Connection) -> Result<(), PeerError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS peers (
            peer_id TEXT PRIMARY KEY,
            handle TEXT NOT NULL UNIQUE,
            status TEXT NOT NULL,
            reason TEXT,
            updated_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_peers_status ON peers (status);",
    )
    .map_err(PeerError::Storage)
}

/// Repository managing peer records in SQLite.
#[derive(Clone)]
pub struct PeersRepo {
    db: Database,
}

impl PeersRepo {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn upsert(&self, peer: &Peer) -> Result<(), PeerError> {
        let peer_id = peer.peer_id.to_hex();
        let handle = peer.handle.as_str().to_string();
        let (status, reason) = encode_status(&peer.status);
        let updated_at_ms = peer.updated_at.as_millis();

        self.db
            .exec(move |conn| {
                conn.execute(
                    "INSERT INTO peers (peer_id, handle, status, reason, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(peer_id) DO UPDATE SET
                        handle = excluded.handle,
                        status = excluded.status,
                        reason = excluded.reason,
                        updated_at_ms = excluded.updated_at_ms",
                    params![peer_id, handle, status, reason, updated_at_ms],
                )
                .map_err(PeerError::Storage)?;
                Ok(())
            })
            .await
            .map_err(PeerError::Database)?
    }

    pub async fn get_by_id(&self, peer_id: PeerId) -> Result<Option<Peer>, PeerError> {
        let key = peer_id.to_hex();

        self.db
            .exec(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT peer_id, handle, status, reason, updated_at_ms
                         FROM peers
                         WHERE peer_id = ?1",
                        params![key],
                        read_raw_row,
                    )
                    .optional()
                    .map_err(PeerError::Storage)?;
                row.map(decode_row).transpose()
            })
            .await
            .map_err(PeerError::Database)?
    }

    pub async fn get_by_handle(&self, handle: &str) -> Result<Option<Peer>, PeerError> {
        let key = handle.to_string();

        self.db
            .exec(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT peer_id, handle, status, reason, updated_at_ms
                         FROM peers
                         WHERE handle = ?1",
                        params![key],
                        read_raw_row,
                    )
                    .optional()
                    .map_err(PeerError::Storage)?;
                row.map(decode_row).transpose()
            })
            .await
            .map_err(PeerError::Database)?
    }

    pub async fn list_all(&self) -> Result<Vec<Peer>, PeerError> {
        self.db
            .exec(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT peer_id, handle, status, reason, updated_at_ms
                         FROM peers
                         ORDER BY handle ASC",
                    )
                    .map_err(PeerError::Storage)?;
                let rows = stmt
                    .query_map([], read_raw_row)
                    .map_err(PeerError::Storage)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(PeerError::Storage)?;
                rows.into_iter().map(decode_row).collect()
            })
            .await
            .map_err(PeerError::Database)?
    }

    pub async fn list_pending_inbound(&self) -> Result<Vec<Peer>, PeerError> {
        self.list_by_status("pending_inbound").await
    }

    pub async fn list_approved(&self) -> Result<Vec<Peer>, PeerError> {
        self.list_by_status("approved").await
    }

    async fn list_by_status(&self, status: &'static str) -> Result<Vec<Peer>, PeerError> {
        self.db
            .exec(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT peer_id, handle, status, reason, updated_at_ms
                         FROM peers
                         WHERE status = ?1
                         ORDER BY handle ASC",
                    )
                    .map_err(PeerError::Storage)?;
                let rows = stmt
                    .query_map(params![status], read_raw_row)
                    .map_err(PeerError::Storage)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(PeerError::Storage)?;
                rows.into_iter().map(decode_row).collect()
            })
            .await
            .map_err(PeerError::Database)?
    }
}

fn read_raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawPeerRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn encode_status(status: &PeerStatus) -> (&'static str, Option<String>) {
    match status {
        PeerStatus::PendingInbound { reason } => ("pending_inbound", Some(reason.clone())),
        PeerStatus::PendingOutbound { reason } => ("pending_outbound", Some(reason.clone())),
        PeerStatus::Approved => ("approved", None),
        PeerStatus::Revoked => ("revoked", None),
    }
}

fn decode_row(raw: RawPeerRow) -> Result<Peer, PeerError> {
    let (peer_id_str, handle_str, status_str, reason, updated_at_ms) = raw;

    let peer_id = peer_id_str
        .parse::<PeerId>()
        .map_err(|_| PeerError::CorruptRecord(format!("invalid peer_id '{peer_id_str}'")))?;

    let handle = PeerHandle::new(&handle_str)
        .map_err(|_| PeerError::CorruptRecord(format!("invalid handle '{handle_str}'")))?;

    let status = match status_str.as_str() {
        "pending_inbound" => PeerStatus::PendingInbound {
            reason: reason.unwrap_or_default(),
        },
        "pending_outbound" => PeerStatus::PendingOutbound {
            reason: reason.unwrap_or_default(),
        },
        "approved" => PeerStatus::Approved,
        "revoked" => PeerStatus::Revoked,
        other => {
            return Err(PeerError::CorruptRecord(format!(
                "unknown peer status '{other}'"
            )));
        }
    };

    Ok(Peer {
        peer_id,
        handle,
        status,
        updated_at: Timestamp::from_millis(updated_at_ms),
    })
}
