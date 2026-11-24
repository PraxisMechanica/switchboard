//! Extensions vertical slice: callable local workspaces, `AgentHarness` port, and Codex adapter.

pub mod cli;
pub mod codex;
pub mod codex_policy;
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
    CodexSessionInfo, DeliveryOutcome, DeliveryRequest, Extension, ExtensionName, HarnessKind,
};
pub use repo::{migrate, ExtensionsRepo};

/// Errors returned by the extensions domain and agent harness adapters.
#[derive(Debug, Error)]
pub enum ExtensionError {
    #[error(
        "invalid extension name '{0}': must be 1..=48 lowercase ASCII alphanumeric, '-', or '_'"
    )]
    InvalidName(String),

    #[error("unsupported agent harness '{0}'")]
    UnsupportedHarness(String),

    #[error("invalid workspace directory: path cannot be empty")]
    InvalidWorkspaceDir,

    #[error("extension '{0}' not found")]
    NotFound(String),

    #[error("no pre-existing agent session bound for '{0}'; bind one with `switchboard ext bind` or call <peer>/<session_id> directly")]
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
        let (name, workspace_dir, session_id) = match self.repo.get(ext_name).await? {
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
                let fallback_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                (ext_name.clone(), fallback_dir, sid)
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::primitives::FixedClock;

    use super::*;

    #[test]
    fn extension_name_validation_and_serde_roundtrip() {
        let ok = ExtensionName::new("backend_api-01").unwrap();
        assert_eq!(ok.as_str(), "backend_api-01");
        assert_eq!(ok.to_string(), "backend_api-01");
        assert_eq!(ExtensionName::from_str("backend_api-01").unwrap(), ok);

        let json = serde_json::to_string(&ok).unwrap();
        assert_eq!(json, "\"backend_api-01\"");
        let parsed: ExtensionName = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ok);

        assert!(matches!(
            ExtensionName::new(""),
            Err(ExtensionError::InvalidName(_))
        ));
        assert!(matches!(
            ExtensionName::new("Upper"),
            Err(ExtensionError::InvalidName(_))
        ));
        assert!(matches!(
            ExtensionName::new("has space"),
            Err(ExtensionError::InvalidName(_))
        ));
        let too_long = "a".repeat(49);
        assert!(matches!(
            ExtensionName::new(&too_long),
            Err(ExtensionError::InvalidName(_))
        ));
        assert!(ExtensionName::new(&"a".repeat(48)).is_ok());
    }

    #[test]
    fn harness_kind_roundtrips() {
        assert_eq!(HarnessKind::Codex.to_string(), "codex");
        assert_eq!(HarnessKind::from_str("codex").unwrap(), HarnessKind::Codex);
        assert_eq!(
            serde_json::to_string(&HarnessKind::Codex).unwrap(),
            "\"codex\""
        );
        assert!(matches!(
            HarnessKind::from_str("unknown"),
            Err(ExtensionError::UnsupportedHarness(_))
        ));
    }

    #[tokio::test]
    async fn extensions_service_crud_and_deliver_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        db.with_conn_sync(|c| migrate(c).unwrap());

        let harness = StubHarness::with_replies(["first answer", "second answer"]);
        let clock = FixedClock::new(1_700_000_000_000);
        let svc = ExtensionsService::new(db, harness.clone(), clock);

        let name = ExtensionName::new("backend").unwrap();
        let added = svc
            .add(
                name.clone(),
                PathBuf::from("/tmp/workspace-a"),
                "Backend repo".to_string(),
                HarnessKind::Codex,
                Some("019a20e4-80cc-7754-b584-33393adcc1f5".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(added.name, name);
        assert_eq!(svc.list().await.unwrap().len(), 1);
        assert_eq!(svc.get(&name).await.unwrap(), added);

        let first = svc
            .deliver(
                &name,
                Uuid::now_v7(),
                "alice".to_string(),
                "ping".to_string(),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let sid = match first {
            DeliveryOutcome::Replied { body, session_id } => {
                assert_eq!(body, "first answer");
                session_id.unwrap()
            }
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(sid, "019a20e4-80cc-7754-b584-33393adcc1f5");

        let second = svc
            .deliver(
                &name,
                Uuid::now_v7(),
                "alice".to_string(),
                "follow-up".to_string(),
                Some(sid.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            second,
            DeliveryOutcome::Replied {
                body: "second answer".to_string(),
                session_id: Some(sid),
            }
        );

        // Unbound extension refuses to create a new session.
        let unbound = ExtensionName::new("unbound").unwrap();
        svc.add(
            unbound.clone(),
            PathBuf::from("/tmp/workspace-b"),
            "Unbound".to_string(),
            HarnessKind::Codex,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            svc.deliver(
                &unbound,
                Uuid::now_v7(),
                "alice".to_string(),
                "ping".to_string(),
                None,
                CancellationToken::new(),
            )
            .await,
            Err(ExtensionError::SessionRequired(_))
        ));

        // Direct arbitrary pre-existing session UUID works without registration.
        let direct_uuid = ExtensionName::new("01a20e4a-a5b2-77e0-bd3f-2e46d42b8ebd").unwrap();
        let direct = svc
            .deliver(
                &direct_uuid,
                Uuid::now_v7(),
                "alice".to_string(),
                "direct ping".to_string(),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            direct,
            DeliveryOutcome::Replied { session_id: Some(ref s), .. }
                if s == "01a20e4a-a5b2-77e0-bd3f-2e46d42b8ebd"
        ));

        assert!(svc.remove(&name).await.unwrap());
        assert!(!svc.remove(&name).await.unwrap());
        assert!(matches!(
            svc.get(&name).await,
            Err(ExtensionError::NotFound(_))
        ));
    }
}
