//! Daemon service vertical slice: user-level `launchd` and `systemd` unit installation.

pub mod cli;
pub mod launchd;
pub mod systemd;
pub mod updater;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub use cli::ServiceCmd;
pub use launchd::LaunchdAdapter;
pub use systemd::SystemdUserAdapter;
pub use updater::{check_and_apply_update, is_dev_build_path, UpdateConfig, UpdateOutcome};

/// Default service label used for user-level daemon registration.
pub const DEFAULT_SERVICE_LABEL: &str = "com.praxis.switchboard";

/// Declarative specification used to render and install a user daemon service unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub label: String,
    pub binary_path: PathBuf,
    pub config_path: Option<PathBuf>,
    pub log_dir: PathBuf,
}

/// Reported installation status of a platform service unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub platform: String,
    pub unit_path: PathBuf,
    pub installed: bool,
}

/// Errors returned while resolving, validating, installing, or removing service units.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("unsupported host platform for service installation: {0}")]
    UnsupportedPlatform(String),

    #[error("invalid service path or specification: {0}")]
    InvalidPath(String),

    #[error("service I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Port trait abstracting platform-specific user service unit management.
pub trait ServiceUnitAdapter: Send + Sync {
    fn platform_name(&self) -> &'static str;

    fn unit_path(&self) -> PathBuf;

    fn render(&self, spec: &ServiceSpec) -> String;

    fn install(&self, spec: &ServiceSpec) -> Result<ServiceStatus, ServiceError>;

    fn uninstall(&self) -> Result<ServiceStatus, ServiceError>;

    fn status(&self) -> ServiceStatus;
}

/// Returns the default user-level [`ServiceUnitAdapter`] for the current host OS.
pub fn default_adapter_for_host() -> Result<Box<dyn ServiceUnitAdapter>, ServiceError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(LaunchdAdapter::default_user()?))
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(SystemdUserAdapter::default_user()?))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(ServiceError::UnsupportedPlatform(
            std::env::consts::OS.to_string(),
        ))
    }
}

pub(crate) fn user_home_dir() -> Result<PathBuf, ServiceError> {
    match std::env::var_os("HOME") {
        Some(val) if !val.is_empty() => Ok(PathBuf::from(val)),
        _ => Err(ServiceError::InvalidPath(
            "HOME environment variable is not set".to_string(),
        )),
    }
}

pub(crate) fn validate_spec(spec: &ServiceSpec) -> Result<(), ServiceError> {
    if spec.label.trim().is_empty() {
        return Err(ServiceError::InvalidPath(
            "service label cannot be empty".to_string(),
        ));
    }
    if spec.label.chars().any(|c| matches!(c, '\0' | '\r' | '\n')) {
        return Err(ServiceError::InvalidPath(
            "service label cannot contain control characters".to_string(),
        ));
    }

    validate_path_field("binary_path", &spec.binary_path)?;
    validate_path_field("log_dir", &spec.log_dir)?;
    if let Some(config_path) = &spec.config_path {
        validate_path_field("config_path", config_path)?;
    }

    Ok(())
}

fn validate_path_field(field: &str, path: &Path) -> Result<(), ServiceError> {
    if path.as_os_str().is_empty() {
        return Err(ServiceError::InvalidPath(format!(
            "{field} cannot be empty"
        )));
    }
    let Some(text) = path.to_str() else {
        return Err(ServiceError::InvalidPath(format!(
            "{field} must be valid UTF-8"
        )));
    };
    if text.chars().any(|c| matches!(c, '\0' | '\r' | '\n')) {
        return Err(ServiceError::InvalidPath(format!(
            "{field} cannot contain NUL or newline characters"
        )));
    }
    Ok(())
}

pub(crate) fn write_file_atomic(target: &Path, contents: &str) -> Result<(), ServiceError> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            ServiceError::InvalidPath(format!(
                "unit path '{}' must have a parent directory",
                target.display()
            ))
        })?;
    let file_name = target.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        ServiceError::InvalidPath(format!(
            "unit path '{}' must end with a valid UTF-8 file name",
            target.display()
        ))
    })?;

    fs::create_dir_all(parent)?;

    let tmp_name = format!(".{file_name}.tmp.{}.{}", std::process::id(), Uuid::now_v7());
    let tmp_path = parent.join(tmp_name);

    let write_result = (|| -> Result<(), std::io::Error> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(ServiceError::Io(err));
    }

    if let Err(err) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ServiceError::Io(err));
    }

    Ok(())
}

pub(crate) fn install_unit(
    adapter: &dyn ServiceUnitAdapter,
    spec: &ServiceSpec,
) -> Result<ServiceStatus, ServiceError> {
    validate_spec(spec)?;
    let unit_path = adapter.unit_path();
    fs::create_dir_all(&spec.log_dir)?;
    let rendered = adapter.render(spec);
    write_file_atomic(&unit_path, &rendered)?;

    Ok(ServiceStatus {
        platform: adapter.platform_name().to_string(),
        unit_path,
        installed: true,
    })
}

pub(crate) fn uninstall_unit(
    adapter: &dyn ServiceUnitAdapter,
) -> Result<ServiceStatus, ServiceError> {
    let unit_path = adapter.unit_path();
    match fs::remove_file(&unit_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(ServiceError::Io(err)),
    }

    Ok(ServiceStatus {
        platform: adapter.platform_name().to_string(),
        unit_path,
        installed: false,
    })
}

pub(crate) fn unit_status(adapter: &dyn ServiceUnitAdapter) -> ServiceStatus {
    let unit_path = adapter.unit_path();
    let installed = unit_path.is_file();
    ServiceStatus {
        platform: adapter.platform_name().to_string(),
        unit_path,
        installed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_status_json_roundtrip() {
        let status = ServiceStatus {
            platform: "launchd".to_string(),
            unit_path: PathBuf::from("/tmp/com.praxis.switchboard.plist"),
            installed: true,
        };
        let encoded = serde_json::to_string(&status).unwrap();
        let decoded: ServiceStatus = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, status);
    }

    #[test]
    fn validate_spec_rejects_invalid_inputs() {
        let base = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::from("/usr/local/bin/switchboard"),
            config_path: Some(PathBuf::from("/etc/switchboard/config.toml")),
            log_dir: PathBuf::from("/var/log/switchboard"),
        };
        assert!(validate_spec(&base).is_ok());

        let mut empty_label = base.clone();
        empty_label.label = "   ".to_string();
        assert!(matches!(
            validate_spec(&empty_label),
            Err(ServiceError::InvalidPath(_))
        ));

        let mut newline_label = base.clone();
        newline_label.label = "com.praxis\nswitchboard".to_string();
        assert!(matches!(
            validate_spec(&newline_label),
            Err(ServiceError::InvalidPath(_))
        ));

        let mut empty_binary = base.clone();
        empty_binary.binary_path = PathBuf::new();
        assert!(matches!(
            validate_spec(&empty_binary),
            Err(ServiceError::InvalidPath(_))
        ));

        let mut newline_cfg = base.clone();
        newline_cfg.config_path = Some(PathBuf::from("/tmp/bad\nconfig.toml"));
        assert!(matches!(
            validate_spec(&newline_cfg),
            Err(ServiceError::InvalidPath(_))
        ));

        let mut empty_log_dir = base;
        empty_log_dir.log_dir = PathBuf::new();
        assert!(matches!(
            validate_spec(&empty_log_dir),
            Err(ServiceError::InvalidPath(_))
        ));
    }

    #[test]
    fn default_adapter_for_host_matches_platform() {
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let result = default_adapter_for_host();

        #[cfg(target_os = "macos")]
        {
            let adapter = result.unwrap();
            assert_eq!(adapter.platform_name(), launchd::PLATFORM_NAME);
            assert!(adapter
                .unit_path()
                .ends_with("Library/LaunchAgents/com.praxis.switchboard.plist"));
        }

        #[cfg(target_os = "linux")]
        {
            let adapter = result.unwrap();
            assert_eq!(adapter.platform_name(), systemd::PLATFORM_NAME);
            assert!(adapter
                .unit_path()
                .ends_with(".config/systemd/user/switchboard.service"));
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            assert!(matches!(result, Err(ServiceError::UnsupportedPlatform(_))));
        }
    }
}
