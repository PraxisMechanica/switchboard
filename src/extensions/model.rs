//! Domain types for callable local workspace extensions and harness deliveries.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::primitives::Timestamp;

use super::ExtensionError;

/// Validated extension identifier matching `[a-z0-9_-]{1,48}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtensionName(String);

impl ExtensionName {
    /// Constructs a validated extension name.
    pub fn new(raw: &str) -> Result<Self, ExtensionError> {
        if raw.is_empty()
            || raw.len() > 48
            || !raw
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(ExtensionError::InvalidName(raw.to_string()));
        }
        Ok(Self(raw.to_string()))
    }

    /// Returns the underlying validated string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ExtensionName {
    type Err = ExtensionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Supported local agent harness implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    Codex,
}

impl HarnessKind {
    /// Returns the canonical lowercase string name for this harness kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
        }
    }
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessKind {
    type Err = ExtensionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            _ => Err(ExtensionError::UnsupportedHarness(s.to_string())),
        }
    }
}

/// Registered callable local workspace extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extension {
    pub name: ExtensionName,
    pub workspace_dir: PathBuf,
    pub description: String,
    pub harness: HarnessKind,
    pub created_at: Timestamp,
}

/// Input payload passed to an [`AgentHarness`](super::AgentHarness) for one call turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRequest {
    pub call_id: Uuid,
    pub extension: ExtensionName,
    pub workspace_dir: PathBuf,
    pub from_handle: String,
    pub body: String,
    pub existing_session_id: Option<String>,
}

/// Terminal outcome produced by an [`AgentHarness`](super::AgentHarness) delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Replied {
        body: String,
        session_id: Option<String>,
    },
    TimedOut,
    Cancelled,
    ProcessFailed {
        exit_code: Option<i32>,
        detail: String,
    },
}
