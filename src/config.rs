//! Typed TOML configuration validated before the CLI or daemon accepts work.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::extensions::CodexConfig;
use crate::transport::NetworkMode;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write config file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Concrete P2P transport implementation selected at daemon startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    #[default]
    Iroh,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryConfig {
    /// Git remote URL (or local bare repo path) for the signed team phone book.
    pub git_remote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSection {
    #[serde(default = "default_codex_binary")]
    pub binary: PathBuf,

    #[serde(default = "default_codex_args")]
    pub args: Vec<String>,

    #[serde(default = "default_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
}

impl Default for CodexSection {
    fn default() -> Self {
        Self {
            binary: default_codex_binary(),
            args: default_codex_args(),
            turn_timeout_secs: default_turn_timeout_secs(),
        }
    }
}

impl CodexSection {
    pub fn to_codex_config(&self) -> CodexConfig {
        CodexConfig {
            binary: self.binary.clone(),
            args: self.args.clone(),
            turn_timeout: Duration::from_secs(self.turn_timeout_secs),
            codex_home: None,
        }
    }
}

/// Top-level validated configuration for a switchboard node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchboardConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    #[serde(default)]
    pub transport: TransportKind,

    #[serde(default)]
    pub network: NetworkMode,

    #[serde(default)]
    pub directory: DirectoryConfig,

    #[serde(default)]
    pub codex: CodexSection,
}

impl Default for SwitchboardConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            transport: TransportKind::default(),
            network: NetworkMode::GlobalP2p,
            directory: DirectoryConfig::default(),
            codex: CodexSection::default(),
        }
    }
}

impl SwitchboardConfig {
    /// Loads TOML config from an explicit path, `SWITCHBOARD_CONFIG`, or `~/.config/switchboard/config.toml`.
    pub fn load(explicit_path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match explicit_path {
            Some(p) => Some(p.to_path_buf()),
            None => std::env::var_os("SWITCHBOARD_CONFIG")
                .map(PathBuf::from)
                .or_else(default_config_path),
        };

        let Some(path) = path else {
            return Ok(Self::default());
        };

        if !path.exists() {
            if explicit_path.is_some() {
                return Err(ConfigError::Read {
                    path: path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "explicit config file does not exist",
                    ),
                });
            }
            return Ok(Self::default());
        }

        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let cfg: Self =
            toml::from_str(&raw).map_err(|source| ConfigError::Parse { path, source })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.data_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid("data_dir cannot be empty".into()));
        }
        if self.codex.binary.as_os_str().is_empty() {
            return Err(ConfigError::Invalid("codex.binary cannot be empty".into()));
        }
        if self.codex.turn_timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "codex.turn_timeout_secs must be > 0".into(),
            ));
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("switchboard.db")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("daemon.sock")
    }

    pub fn phonebook_dir(&self) -> PathBuf {
        self.data_dir.join("phonebook")
    }

    pub fn codex_home_dir(&self) -> PathBuf {
        self.data_dir.join("codex-home")
    }

    pub fn codex_config(&self) -> CodexConfig {
        let mut cfg = self.codex.to_codex_config();
        cfg.codex_home = Some(self.codex_home_dir());
        cfg
    }

    /// Atomically writes a validated TOML configuration file, creating parent directories as needed.
    pub fn write_sample(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let mut rendered = toml::to_string_pretty(self)?;
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }

        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml");
        let tmp_path = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::now_v7()));

        let write_result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            file.write_all(rendered.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(source) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(ConfigError::Write {
                path: tmp_path,
                source,
            });
        }

        if let Err(source) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(ConfigError::Write {
                path: path.to_path_buf(),
                source,
            });
        }

        Ok(())
    }
}

fn default_codex_binary() -> PathBuf {
    PathBuf::from("codex")
}

fn default_codex_args() -> Vec<String> {
    crate::extensions::model::default_codex_args()
}

fn default_turn_timeout_secs() -> u64 {
    180
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_data_dir() -> PathBuf {
    home_dir().join(".local").join("share").join("switchboard")
}

fn default_config_path() -> Option<PathBuf> {
    Some(
        home_dir()
            .join(".config")
            .join("switchboard")
            .join("config.toml"),
    )
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn default_config_parses_with_iroh_transport() {
        let default_cfg = SwitchboardConfig::default();
        assert_eq!(default_cfg.transport, TransportKind::Iroh);
        assert_eq!(default_cfg.network, NetworkMode::GlobalP2p);
        assert!(default_cfg.validate().is_ok());

        let parsed: SwitchboardConfig =
            toml::from_str("").expect("empty TOML should parse with defaults");
        assert_eq!(parsed.transport, TransportKind::Iroh);
        assert_eq!(parsed.network, NetworkMode::GlobalP2p);
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn parses_iroh_transport_and_vpn_only_mode() {
        let raw = r#"
data_dir = "/tmp/switchboard-vpn"
transport = "iroh"

[network.VpnOnly]
bind_addr = "127.0.0.1:4433"
custom_relays = ["https://relay.internal.example"]

[directory]
git_remote = "git@github.com:example/phonebook.git"
"#;

        let cfg: SwitchboardConfig =
            toml::from_str(raw).expect("valid iroh + VpnOnly TOML should parse");
        assert_eq!(cfg.transport, TransportKind::Iroh);
        let expected_addr: SocketAddr = "127.0.0.1:4433".parse().expect("valid socket addr");
        assert_eq!(
            cfg.network,
            NetworkMode::VpnOnly {
                bind_addr: Some(expected_addr),
                custom_relays: vec!["https://relay.internal.example".to_string()],
            }
        );
        assert_eq!(
            cfg.directory.git_remote.as_deref(),
            Some("git@github.com:example/phonebook.git")
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn unknown_transport_fails_deserialization_cleanly() {
        let err = toml::from_str::<SwitchboardConfig>(r#"transport = "bogus""#)
            .expect_err("unknown transport must fail deserialization");
        let msg = err.to_string();
        assert!(
            msg.contains("bogus") || msg.contains("unknown variant"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn write_sample_creates_parent_dirs_and_round_trips() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir
            .path()
            .join("nested")
            .join("switchboard")
            .join("config.toml");

        let cfg = SwitchboardConfig {
            data_dir: dir.path().join("data"),
            transport: TransportKind::Iroh,
            network: NetworkMode::VpnOnly {
                bind_addr: Some("127.0.0.1:4433".parse().expect("valid socket addr")),
                custom_relays: vec!["https://relay.internal.example".to_string()],
            },
            directory: DirectoryConfig {
                git_remote: Some("https://example.com/team/phonebook.git".to_string()),
            },
            ..SwitchboardConfig::default()
        };

        cfg.write_sample(&config_path)
            .expect("write_sample should succeed");

        let loaded =
            SwitchboardConfig::load(Some(&config_path)).expect("written config should reload");
        assert_eq!(loaded.data_dir, cfg.data_dir);
        assert_eq!(loaded.transport, TransportKind::Iroh);
        assert_eq!(loaded.network, cfg.network);
        assert_eq!(loaded.directory.git_remote, cfg.directory.git_remote);

        let parent = config_path.parent().expect("parent dir");
        let entries: Vec<_> = std::fs::read_dir(parent)
            .expect("read parent dir")
            .collect::<Result<Vec<_>, _>>()
            .expect("dir entries");
        assert_eq!(entries.len(), 1, "no temporary files should remain");
    }
}
