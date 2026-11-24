//! SQLite persistence and idempotency receipt storage for the calls vertical slice.

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::Database;
use crate::extensions::ExtensionName;
use crate::primitives::{Digest, ReceiptRecord, ReceiptStore, Timestamp};

use super::model::{
    CallDirection, CallId, CallMessageRecord, CallRecord, CallResponse, CallStatus, MessageId,
};
use super::CallError;

/// Runs the calls domain's SQLite schema migration.
pub fn migrate(conn: &Connection) -> Result<(), CallError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS calls (
            call_id TEXT PRIMARY KEY,
            peer_id TEXT NOT NULL,
            peer_handle TEXT NOT NULL,
            extension TEXT NOT NULL,
            direction TEXT NOT NULL,
            status TEXT NOT NULL,
            harness_session_id TEXT,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS call_messages (
            message_id TEXT PRIMARY KEY,
            call_id TEXT NOT NULL,
            sender_handle TEXT NOT NULL,
            request_hash TEXT NOT NULL,
            request_body TEXT NOT NULL,
            response_json TEXT,
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(call_id) REFERENCES calls(call_id)
        );
        CREATE INDEX IF NOT EXISTS idx_call_messages_call_id
            ON call_messages (call_id, created_at_ms);",
    )
    .map_err(CallError::Storage)
}

#[derive(Clone)]
pub struct CallsRepo {
    db: Database,
}

impl CallsRepo {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn upsert_call(&self, call: &CallRecord) -> Result<(), CallError> {
        let call = call.clone();
        self.db
            .exec(move |conn| {
                conn.execute(
                    "INSERT INTO calls (
                        call_id, peer_id, peer_handle, extension, direction, status,
                        harness_session_id, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(call_id) DO UPDATE SET
                        status = excluded.status,
                        harness_session_id = COALESCE(excluded.harness_session_id, calls.harness_session_id),
                        updated_at_ms = excluded.updated_at_ms",
                    params![
                        call.call_id.to_string(),
                        call.peer_id.to_hex(),
                        call.peer_handle,
                        call.extension.as_str(),
                        call.direction.as_str(),
                        call.status.as_str(),
                        call.harness_session_id,
                        call.created_at.as_millis(),
                        call.updated_at.as_millis(),
                    ],
                )
                .map_err(CallError::Storage)?;
                Ok(())
            })
            .await
            .map_err(CallError::Database)?
    }

    pub async fn get_call(&self, call_id: CallId) -> Result<Option<CallRecord>, CallError> {
        let id = call_id.to_string();
        self.db
            .exec(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT call_id, peer_id, peer_handle, extension, direction, status,
                                harness_session_id, created_at_ms, updated_at_ms
                         FROM calls WHERE call_id = ?1",
                        params![id],
                        decode_call_row,
                    )
                    .optional()
                    .map_err(CallError::Storage)?;
                row.transpose()
            })
            .await
            .map_err(CallError::Database)?
    }

    pub async fn update_session_id(
        &self,
        call_id: CallId,
        session_id: String,
        updated_at: Timestamp,
    ) -> Result<(), CallError> {
        let id = call_id.to_string();
        let ts = updated_at.as_millis();
        self.db
            .exec(move |conn| {
                conn.execute(
                    "UPDATE calls SET harness_session_id = ?1, updated_at_ms = ?2 WHERE call_id = ?3",
                    params![session_id, ts, id],
                )
                .map_err(CallError::Storage)?;
                Ok(())
            })
            .await
            .map_err(CallError::Database)?
    }

    pub async fn set_status(
        &self,
        call_id: CallId,
        status: CallStatus,
        updated_at: Timestamp,
    ) -> Result<(), CallError> {
        let id = call_id.to_string();
        let st = status.as_str();
        let ts = updated_at.as_millis();
        self.db
            .exec(move |conn| {
                conn.execute(
                    "UPDATE calls SET status = ?1, updated_at_ms = ?2 WHERE call_id = ?3",
                    params![st, ts, id],
                )
                .map_err(CallError::Storage)?;
                Ok(())
            })
            .await
            .map_err(CallError::Database)?
    }

    pub async fn insert_message(&self, msg: &CallMessageRecord) -> Result<(), CallError> {
        let msg = msg.clone();
        let response_json = msg
            .response
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(CallError::Serialization)?;

        self.db
            .exec(move |conn| {
                conn.execute(
                    "INSERT INTO call_messages (
                        message_id, call_id, sender_handle, request_hash,
                        request_body, response_json, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(message_id) DO UPDATE SET
                        response_json = COALESCE(excluded.response_json, call_messages.response_json)",
                    params![
                        msg.message_id.to_string(),
                        msg.call_id.to_string(),
                        msg.sender_handle,
                        msg.request_hash.to_hex(),
                        msg.request_body,
                        response_json,
                        msg.created_at.as_millis(),
                    ],
                )
                .map_err(CallError::Storage)?;
                Ok(())
            })
            .await
            .map_err(CallError::Database)?
    }

    pub async fn list_messages(
        &self,
        call_id: CallId,
    ) -> Result<Vec<CallMessageRecord>, CallError> {
        let id = call_id.to_string();
        self.db
            .exec(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT message_id, call_id, sender_handle, request_hash,
                                request_body, response_json, created_at_ms
                         FROM call_messages
                         WHERE call_id = ?1
                         ORDER BY created_at_ms ASC",
                    )
                    .map_err(CallError::Storage)?;
                let rows = stmt
                    .query_map(params![id], decode_message_row)
                    .map_err(CallError::Storage)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(CallError::Storage)??);
                }
                Ok(out)
            })
            .await
            .map_err(CallError::Database)?
    }
}

#[async_trait::async_trait]
impl ReceiptStore<MessageId, CallResponse> for CallsRepo {
    async fn load_receipt(
        &self,
        key: &MessageId,
    ) -> Result<Option<ReceiptRecord<CallResponse>>, String> {
        let id = key.to_string();
        self.db
            .exec(move |conn| {
                let row: Option<(String, Option<String>)> = conn
                    .query_row(
                        "SELECT request_hash, response_json FROM call_messages WHERE message_id = ?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;

                let Some((hash_hex, Some(resp_json))) = row else {
                    return Ok(None);
                };
                let request_hash: Digest = hash_hex.parse().map_err(|e| format!("{e}"))?;
                let response: CallResponse =
                    serde_json::from_str(&resp_json).map_err(|e| e.to_string())?;
                Ok(Some(ReceiptRecord {
                    request_hash,
                    response,
                }))
            })
            .await
            .map_err(|e| e.to_string())?
    }

    async fn save_receipt(
        &self,
        key: &MessageId,
        request_hash: Digest,
        response: &CallResponse,
    ) -> Result<(), String> {
        let id = key.to_string();
        let hash_hex = request_hash.to_hex();
        let resp_json = serde_json::to_string(response).map_err(|e| e.to_string())?;

        self.db
            .exec(move |conn| {
                let updated = conn
                    .execute(
                        "UPDATE call_messages SET response_json = ?1, request_hash = ?2 WHERE message_id = ?3",
                        params![resp_json, hash_hex, id],
                    )
                    .map_err(|e| e.to_string())?;
                if updated == 0 {
                    return Err("call_messages row missing before saving receipt".to_string());
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())?
    }
}

fn decode_call_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<CallRecord, CallError>> {
    let call_id_raw: String = row.get(0)?;
    let peer_id_raw: String = row.get(1)?;
    let peer_handle: String = row.get(2)?;
    let ext_raw: String = row.get(3)?;
    let dir_raw: String = row.get(4)?;
    let status_raw: String = row.get(5)?;
    let harness_session_id: Option<String> = row.get(6)?;
    let created_at_ms: i64 = row.get(7)?;
    let updated_at_ms: i64 = row.get(8)?;

    Ok((|| {
        Ok(CallRecord {
            call_id: call_id_raw.parse()?,
            peer_id: peer_id_raw
                .parse()
                .map_err(|e| CallError::CorruptRecord(format!("peer_id: {e}")))?,
            peer_handle,
            extension: ExtensionName::new(&ext_raw)
                .map_err(|e| CallError::CorruptRecord(e.to_string()))?,
            direction: CallDirection::parse_str(&dir_raw)?,
            status: CallStatus::parse_str(&status_raw)?,
            harness_session_id,
            created_at: Timestamp::from_millis(created_at_ms),
            updated_at: Timestamp::from_millis(updated_at_ms),
        })
    })())
}

fn decode_message_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<CallMessageRecord, CallError>> {
    let msg_id_raw: String = row.get(0)?;
    let call_id_raw: String = row.get(1)?;
    let sender_handle: String = row.get(2)?;
    let hash_raw: String = row.get(3)?;
    let request_body: String = row.get(4)?;
    let response_raw: Option<String> = row.get(5)?;
    let created_at_ms: i64 = row.get(6)?;

    Ok((|| {
        let response = response_raw
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(CallError::Serialization)?;
        Ok(CallMessageRecord {
            message_id: msg_id_raw.parse()?,
            call_id: call_id_raw.parse()?,
            sender_handle,
            request_hash: hash_raw
                .parse()
                .map_err(|e| CallError::CorruptRecord(format!("request_hash: {e}")))?,
            request_body,
            response,
            created_at: Timestamp::from_millis(created_at_ms),
        })
    })())
}
