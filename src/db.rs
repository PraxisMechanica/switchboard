//! Thin SQLite connection handle. Each vertical domain owns its own migrations and queries.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use thiserror::Error;

/// Shared SQLite database handle keeping blocking I/O off Tokio worker threads.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Error)]
pub enum DbError {
    #[error("failed to open sqlite database: {0}")]
    Open(#[from] rusqlite::Error),

    #[error("database task panicked: {0}")]
    Join(String),
}

impl Database {
    /// Opens a file-backed SQLite database in WAL mode with foreign keys enabled.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                DbError::Open(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(err.to_string()),
                ))
            })?;
        }
        let conn = Connection::open(path)?;
        configure(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens an isolated in-memory SQLite database for deterministic tests.
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs a synchronous closure against the underlying connection on a blocking thread.
    pub async fn exec<T, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> T + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(|err| DbError::Join(err.to_string()))
    }

    /// Runs a synchronous closure directly (intended for startup schema migrations).
    pub fn with_conn_sync<T, F>(&self, f: F) -> T
    where
        F: FnOnce(&mut Connection) -> T,
    {
        let mut guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }
}

fn configure(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )
}
