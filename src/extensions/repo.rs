//! SQLite persistence for registered local workspace extensions.

use std::path::PathBuf;
use std::str::FromStr;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::Database;
use crate::primitives::Timestamp;

use super::model::{Extension, ExtensionName, HarnessKind};
use super::ExtensionError;

/// Runs the extensions domain's SQLite schema migration.
pub fn migrate(conn: &Connection) -> Result<(), ExtensionError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS extensions (
            name TEXT PRIMARY KEY,
            workspace_dir TEXT NOT NULL,
            description TEXT NOT NULL,
            harness TEXT NOT NULL,
            session_id TEXT,
            created_at_ms INTEGER NOT NULL
        );",
    )
    .map_err(ExtensionError::Storage)?;

    let mut stmt = conn
        .prepare("PRAGMA table_info(extensions)")
        .map_err(ExtensionError::Storage)?;
    let has_session_col = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(ExtensionError::Storage)?
        .filter_map(Result::ok)
        .any(|col| col == "session_id");
    drop(stmt);

    if !has_session_col {
        conn.execute("ALTER TABLE extensions ADD COLUMN session_id TEXT", [])
            .map_err(ExtensionError::Storage)?;
    }

    Ok(())
}

/// Repository owning CRUD persistence for [`Extension`] records.
#[derive(Clone)]
pub struct ExtensionsRepo {
    db: Database,
}

impl ExtensionsRepo {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn upsert(&self, ext: &Extension) -> Result<(), ExtensionError> {
        let name = ext.name.as_str().to_string();
        let workspace_dir = ext.workspace_dir.to_string_lossy().into_owned();
        let description = ext.description.clone();
        let harness = ext.harness.as_str().to_string();
        let session_id = ext.session_id.clone();
        let created_at_ms = ext.created_at.as_millis();

        self.db
            .exec(move |conn| {
                conn.execute(
                    "INSERT INTO extensions (name, workspace_dir, description, harness, session_id, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(name) DO UPDATE SET
                        workspace_dir = excluded.workspace_dir,
                        description = excluded.description,
                        harness = excluded.harness,
                        session_id = excluded.session_id,
                        created_at_ms = excluded.created_at_ms",
                    params![name, workspace_dir, description, harness, session_id, created_at_ms],
                )
                .map_err(ExtensionError::Storage)?;
                Ok(())
            })
            .await
            .map_err(ExtensionError::Database)?
    }

    pub async fn bind_session(
        &self,
        name: &ExtensionName,
        session_id: &str,
    ) -> Result<bool, ExtensionError> {
        let key = name.as_str().to_string();
        let sid = session_id.to_string();
        self.db
            .exec(move |conn| {
                let affected = conn
                    .execute(
                        "UPDATE extensions SET session_id = ?1 WHERE name = ?2",
                        params![sid, key],
                    )
                    .map_err(ExtensionError::Storage)?;
                Ok(affected > 0)
            })
            .await
            .map_err(ExtensionError::Database)?
    }

    pub async fn get(&self, name: &ExtensionName) -> Result<Option<Extension>, ExtensionError> {
        let key = name.as_str().to_string();

        self.db
            .exec(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT name, workspace_dir, description, harness, session_id, created_at_ms
                         FROM extensions
                         WHERE name = ?1",
                        params![key],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, Option<String>>(4)?,
                                r.get::<_, i64>(5)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(ExtensionError::Storage)?;

                match row {
                    None => Ok(None),
                    Some((
                        name,
                        workspace_dir,
                        description,
                        harness,
                        session_id,
                        created_at_ms,
                    )) => Ok(Some(parse_row(
                        name,
                        workspace_dir,
                        description,
                        harness,
                        session_id,
                        created_at_ms,
                    )?)),
                }
            })
            .await
            .map_err(ExtensionError::Database)?
    }

    pub async fn list(&self) -> Result<Vec<Extension>, ExtensionError> {
        self.db
            .exec(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT name, workspace_dir, description, harness, session_id, created_at_ms
                         FROM extensions
                         ORDER BY name ASC",
                    )
                    .map_err(ExtensionError::Storage)?;

                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    })
                    .map_err(ExtensionError::Storage)?;

                let mut extensions = Vec::new();
                for row in rows {
                    let (name, workspace_dir, description, harness, session_id, created_at_ms) =
                        row.map_err(ExtensionError::Storage)?;
                    extensions.push(parse_row(
                        name,
                        workspace_dir,
                        description,
                        harness,
                        session_id,
                        created_at_ms,
                    )?);
                }

                Ok(extensions)
            })
            .await
            .map_err(ExtensionError::Database)?
    }

    pub async fn remove(&self, name: &ExtensionName) -> Result<bool, ExtensionError> {
        let key = name.as_str().to_string();

        self.db
            .exec(move |conn| {
                let affected = conn
                    .execute("DELETE FROM extensions WHERE name = ?1", params![key])
                    .map_err(ExtensionError::Storage)?;
                Ok(affected > 0)
            })
            .await
            .map_err(ExtensionError::Database)?
    }
}

fn parse_row(
    raw_name: String,
    workspace_dir: String,
    description: String,
    raw_harness: String,
    session_id: Option<String>,
    created_at_ms: i64,
) -> Result<Extension, ExtensionError> {
    let name = ExtensionName::new(&raw_name)
        .map_err(|_| ExtensionError::CorruptData(format!("invalid extension name '{raw_name}'")))?;
    let harness = HarnessKind::from_str(&raw_harness)
        .map_err(|_| ExtensionError::CorruptData(format!("invalid harness '{raw_harness}'")))?;

    Ok(Extension {
        name,
        workspace_dir: PathBuf::from(workspace_dir),
        description,
        harness,
        session_id,
        created_at: Timestamp::from_millis(created_at_ms),
    })
}
