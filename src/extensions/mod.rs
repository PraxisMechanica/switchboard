//! Extensions vertical slice: callable local workspaces, `AgentHarness` port, and Codex adapter.

pub mod cli;
pub mod codex;
pub mod codex_policy;
pub mod codex_queue;
pub mod conformance;
pub mod model;
pub mod repo;

use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::db::{Database, DbError};
use crate::primitives::Clock;

pub use cli::{run_ext_cmd, ExtCmd};
pub use codex::{CodexConfig, CodexHarness};
pub use conformance::{verify_harness, StubHarness};
pub use model::{
    slugify_title, CodexSessionInfo, DeliveryOutcome, DeliveryRequest, Extension, ExtensionName,
    HarnessKind,
};
pub use repo::{migrate, ExtensionsRepo};

/// Errors returned by the extensions domain and agent harness adapters.
#[derive(Debug, Error)]
pub enum ExtensionError {
    #[error(
        "invalid target '{0}': must be 1..=160 non-control characters without outer whitespace"
    )]
    InvalidName(String),

    #[error("unsupported agent harness '{0}'")]
    UnsupportedHarness(String),

    #[error("invalid workspace directory: path cannot be empty")]
    InvalidWorkspaceDir,

    #[error("extension '{0}' not found")]
    NotFound(String),

    #[error("no pre-existing agent session found matching '{0}'")]
    SessionNotFound(String),

    #[error(
        "multiple pre-existing agent sessions ({1}) match '{0}'; use the session UUID instead"
    )]
    AmbiguousSession(String, usize),

    #[error("no pre-existing agent session bound for '{0}'; call <peer>.<harness>.<conversation-name-or-id> directly")]
    SessionRequired(String),

    #[error("corrupt stored extension data: {0}")]
    CorruptData(String),

    #[error("extension storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("database runtime error: {0}")]
    Database(#[from] DbError),

    #[error("failed to spawn agent harness process: {0}")]
    Spawn(String),

    #[error("agent harness protocol error: {0}")]
    Protocol(String),

    #[error("agent harness I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Port trait abstracting local agent runtimes behind a uniform delivery contract.
#[async_trait::async_trait]
pub trait AgentHarness: Send + Sync {
    async fn deliver(
        &self,
        req: DeliveryRequest,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError>;

    /// Lists pre-existing sessions available in this harness for name/ID resolution.
    async fn list_sessions(&self, _limit: usize) -> Result<Vec<CodexSessionInfo>, ExtensionError> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl<T: AgentHarness + ?Sized> AgentHarness for Arc<T> {
    async fn deliver(
        &self,
        req: DeliveryRequest,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        (**self).deliver(req, cancel).await
    }

    async fn list_sessions(&self, limit: usize) -> Result<Vec<CodexSessionInfo>, ExtensionError> {
        (**self).list_sessions(limit).await
    }
}

/// Application service owning local workspace extension registration and delivery.
#[derive(Clone)]
pub struct ExtensionsService<H: AgentHarness, C: Clock> {
    repo: ExtensionsRepo,
    harness: H,
    clock: C,
}

impl<H: AgentHarness, C: Clock> ExtensionsService<H, C> {
    pub fn new(db: Database, harness: H, clock: C) -> Self {
        Self {
            repo: ExtensionsRepo::new(db),
            harness,
            clock,
        }
    }

    pub async fn add(
        &self,
        name: ExtensionName,
        workspace_dir: PathBuf,
        description: String,
        harness: HarnessKind,
        session_id: Option<String>,
    ) -> Result<Extension, ExtensionError> {
        if workspace_dir.as_os_str().is_empty() {
            return Err(ExtensionError::InvalidWorkspaceDir);
        }

        let clean_session = session_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let ext = Extension {
            name,
            workspace_dir,
            description,
            harness,
            session_id: clean_session,
            created_at: self.clock.now(),
        };
        self.repo.upsert(&ext).await?;
        Ok(ext)
    }

    pub async fn bind_session(
        &self,
        name: &ExtensionName,
        session_id: String,
    ) -> Result<Extension, ExtensionError> {
        let trimmed = session_id.trim();
        if trimmed.is_empty() {
            return Err(ExtensionError::SessionRequired(name.to_string()));
        }
        if !self.repo.bind_session(name, trimmed).await? {
            return Err(ExtensionError::NotFound(name.to_string()));
        }
        self.get(name).await
    }

    pub async fn list(&self) -> Result<Vec<Extension>, ExtensionError> {
        self.repo.list().await
    }

    pub async fn get(&self, name: &ExtensionName) -> Result<Extension, ExtensionError> {
        self.repo
            .get(name)
            .await?
            .ok_or_else(|| ExtensionError::NotFound(name.to_string()))
    }

    pub async fn remove(&self, name: &ExtensionName) -> Result<bool, ExtensionError> {
        self.repo.remove(name).await
    }

    pub async fn deliver(
        &self,
        ext_name: &ExtensionName,
        call_id: Uuid,
        from_handle: String,
        body: String,
        existing_session_id: Option<String>,
        cancel: CancellationToken,
    ) -> Result<DeliveryOutcome, ExtensionError> {
        let fallback_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // Primary path: `<harness>.<conversation-name-or-id>` (zero registration required).
        let (name, workspace_dir, session_id) =
            if let Some((_harness, raw_conv)) = ext_name.parse_harness_target()? {
                let conversation = raw_conv
                    .strip_prefix("codex://threads/")
                    .unwrap_or(raw_conv)
                    .trim();
                if let Some(sid) = existing_session_id {
                    (ext_name.clone(), fallback_dir, sid)
                } else if Uuid::parse_str(conversation).is_ok() {
                    (ext_name.clone(), fallback_dir, conversation.to_string())
                } else {
                    let sessions = self.harness.list_sessions(200).await?;
                    let matched = resolve_session_by_name_or_prefix(&sessions, conversation)?;
                    let dir = if matched.cwd.is_empty() {
                        fallback_dir
                    } else {
                        PathBuf::from(&matched.cwd)
                    };
                    (ext_name.clone(), dir, matched.id.clone())
                }
            } else {
                match self.repo.get(ext_name).await? {
                    Some(ext) => {
                        let sid = existing_session_id
                            .or(ext.session_id)
                            .ok_or_else(|| ExtensionError::SessionRequired(ext.name.to_string()))?;
                        (ext.name, ext.workspace_dir, sid)
                    }
                    None => {
                        let sid = existing_session_id
                            .or_else(|| {
                                Uuid::parse_str(ext_name.as_str())
                                    .ok()
                                    .map(|_| ext_name.to_string())
                            })
                            .ok_or_else(|| ExtensionError::NotFound(ext_name.to_string()))?;
                        (ext_name.clone(), fallback_dir, sid)
                    }
                }
            };

        let req = DeliveryRequest {
            call_id,
            extension: name,
            workspace_dir,
            from_handle,
            body,
            session_id,
        };
        self.harness.deliver(req, cancel).await
    }
}

fn resolve_session_by_name_or_prefix<'a>(
    sessions: &'a [CodexSessionInfo],
    query: &str,
) -> Result<&'a CodexSessionInfo, ExtensionError> {
    let q_lower = query.to_ascii_lowercase();
    let q_slug = slugify_title(query);

    let exact_id: Vec<_> = sessions.iter().filter(|s| s.id == query).collect();
    if exact_id.len() == 1 {
        return Ok(exact_id[0]);
    }

    let exact_title: Vec<_> = sessions
        .iter()
        .filter(|s| {
            !s.title.is_empty()
                && (s.title.to_ascii_lowercase() == q_lower
                    || (!q_slug.is_empty() && slugify_title(&s.title) == q_slug))
        })
        .collect();
    if exact_title.len() == 1 {
        return Ok(exact_title[0]);
    }
    if exact_title.len() > 1 {
        return Err(ExtensionError::AmbiguousSession(
            query.to_string(),
            exact_title.len(),
        ));
    }

    if query.len() >= 8 {
        let prefix: Vec<_> = sessions
            .iter()
            .filter(|s| s.id.to_ascii_lowercase().starts_with(&q_lower))
            .collect();
        if prefix.len() == 1 {
            return Ok(prefix[0]);
        }
        if prefix.len() > 1 {
            return Err(ExtensionError::AmbiguousSession(
                query.to_string(),
                prefix.len(),
            ));
        }
    }

    Err(ExtensionError::SessionNotFound(query.to_string()))
}

#[cfg(test)]
#[path = "extensions_tests.rs"]
mod tests;
