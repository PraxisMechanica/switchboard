//! Linux `systemd --user` unit generator and user-level service adapter.

use std::fmt::Write as _;
use std::path::PathBuf;

use super::{
    install_unit, uninstall_unit, unit_status, user_home_dir, ServiceError, ServiceSpec,
    ServiceStatus, ServiceUnitAdapter,
};

/// Platform identifier reported in [`ServiceStatus::platform`] for Linux `systemd --user`.
pub const PLATFORM_NAME: &str = "systemd";

/// Default systemd user unit filename under `~/.config/systemd/user/`.
pub const DEFAULT_UNIT_NAME: &str = "switchboard.service";

const STDOUT_LOG_FILE: &str = "switchboard.out.log";
const STDERR_LOG_FILE: &str = "switchboard.err.log";

/// User-level Linux `systemd --user` adapter managing a `.service` unit file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUserAdapter {
    base_dir: PathBuf,
    filename: String,
}

impl SystemdUserAdapter {
    /// Constructs an adapter targeting `~/.config/systemd/user/switchboard.service`.
    pub fn default_user() -> Result<Self, ServiceError> {
        let base_dir = user_home_dir()?
            .join(".config")
            .join("systemd")
            .join("user");
        Ok(Self::new(base_dir))
    }

    /// Constructs an adapter rooted in an explicit base directory (primarily for tests).
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filename: DEFAULT_UNIT_NAME.to_string(),
        }
    }

    /// Overrides the unit filename within `base_dir`.
    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = filename.into();
        self
    }
}

impl ServiceUnitAdapter for SystemdUserAdapter {
    fn platform_name(&self) -> &'static str {
        PLATFORM_NAME
    }

    fn unit_path(&self) -> PathBuf {
        self.base_dir.join(&self.filename)
    }

    fn render(&self, spec: &ServiceSpec) -> String {
        let label = sanitize_unit_value(&spec.label);
        let exec_start = render_exec_start(spec);
        let stdout_path = spec.log_dir.join(STDOUT_LOG_FILE);
        let stderr_path = spec.log_dir.join(STDERR_LOG_FILE);
        let stdout_str = sanitize_unit_value(&stdout_path.to_string_lossy());
        let stderr_str = sanitize_unit_value(&stderr_path.to_string_lossy());

        let mut out = String::new();
        let _ = writeln!(out, "[Unit]");
        let _ = writeln!(out, "Description=Switchboard P2P Agent Daemon ({label})");
        let _ = writeln!(out, "After=network-online.target");
        let _ = writeln!(out, "Wants=network-online.target");
        let _ = writeln!(out);
        let _ = writeln!(out, "[Service]");
        let _ = writeln!(out, "Type=simple");
        let _ = writeln!(out, "ExecStart={exec_start}");
        let _ = writeln!(out, "Restart=on-failure");
        let _ = writeln!(out, "RestartSec=5");
        let _ = writeln!(out, "StandardOutput=append:{stdout_str}");
        let _ = writeln!(out, "StandardError=append:{stderr_str}");
        let _ = writeln!(out);
        let _ = writeln!(out, "[Install]");
        let _ = writeln!(out, "WantedBy=default.target");
        out
    }

    fn install(&self, spec: &ServiceSpec) -> Result<ServiceStatus, ServiceError> {
        install_unit(self, spec)
    }

    fn uninstall(&self) -> Result<ServiceStatus, ServiceError> {
        uninstall_unit(self)
    }

    fn status(&self) -> ServiceStatus {
        unit_status(self)
    }
}

/// Renders the `ExecStart=` command line with systemd argument quoting.
pub fn render_exec_start(spec: &ServiceSpec) -> String {
    let mut parts = Vec::with_capacity(4);
    parts.push(quote_exec_arg(&spec.binary_path.to_string_lossy()));
    if let Some(config_path) = &spec.config_path {
        parts.push("--config".to_string());
        parts.push(quote_exec_arg(&config_path.to_string_lossy()));
    }
    parts.push("daemon".to_string());
    parts.join(" ")
}

/// Quotes a single `ExecStart=` argument according to `systemd.service(5)` syntax.
pub fn quote_exec_arg(arg: &str) -> String {
    let is_plain = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | ':' | '@' | '+' | ',')
        });

    if is_plain {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for ch in arg.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn sanitize_unit_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%%"),
            '\r' | '\n' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;
    use crate::service::DEFAULT_SERVICE_LABEL;

    #[test]
    fn quote_exec_arg_handles_plain_and_special_characters() {
        assert_eq!(
            quote_exec_arg("/usr/local/bin/switchboard"),
            "/usr/local/bin/switchboard"
        );
        assert_eq!(quote_exec_arg("--config"), "--config");
        assert_eq!(quote_exec_arg(""), r#""""#);
        assert_eq!(
            quote_exec_arg("/opt/My Apps/switchboard"),
            r#""/opt/My Apps/switchboard""#
        );
        assert_eq!(
            quote_exec_arg(r#"/home/alice/my "cfg"\100%$HOME/config.toml"#),
            r#""/home/alice/my \"cfg\"\\100%%$$HOME/config.toml""#
        );
    }

    #[test]
    fn render_without_config_contains_required_sections_and_directives() {
        let adapter = SystemdUserAdapter::new("/tmp/systemd-user");
        let spec = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::from("/usr/local/bin/switchboard"),
            config_path: None,
            log_dir: PathBuf::from("/home/alice/.local/share/switchboard/logs"),
        };

        let rendered = adapter.render(&spec);
        let expected = concat!(
            "[Unit]\n",
            "Description=Switchboard P2P Agent Daemon (com.praxis.switchboard)\n",
            "After=network-online.target\n",
            "Wants=network-online.target\n",
            "\n",
            "[Service]\n",
            "Type=simple\n",
            "ExecStart=/usr/local/bin/switchboard daemon\n",
            "Restart=on-failure\n",
            "RestartSec=5\n",
            "StandardOutput=append:/home/alice/.local/share/switchboard/logs/switchboard.out.log\n",
            "StandardError=append:/home/alice/.local/share/switchboard/logs/switchboard.err.log\n",
            "\n",
            "[Install]\n",
            "WantedBy=default.target\n",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_with_config_quotes_paths_with_spaces_and_special_chars() {
        let adapter = SystemdUserAdapter::new("/tmp/systemd-user");
        let spec = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::from("/opt/Praxis Apps/switchboard"),
            config_path: Some(PathBuf::from(r#"/home/alice/my "config"/100%.toml"#)),
            log_dir: PathBuf::from("/home/alice/logs/100%"),
        };

        let rendered = adapter.render(&spec);
        assert!(rendered.contains(
            r#"ExecStart="/opt/Praxis Apps/switchboard" --config "/home/alice/my \"config\"/100%%.toml" daemon"#
        ));
        assert!(rendered.contains("Restart=on-failure\n"));
        assert!(rendered.contains("RestartSec=5\n"));
        assert!(rendered.contains("WantedBy=default.target\n"));
        assert!(
            rendered.contains("StandardOutput=append:/home/alice/logs/100%%/switchboard.out.log\n")
        );
        assert!(
            rendered.contains("StandardError=append:/home/alice/logs/100%%/switchboard.err.log\n")
        );
    }

    #[test]
    fn install_status_and_uninstall_lifecycle_in_tempdir() {
        let tmp = tempdir().unwrap();
        let units_dir = tmp.path().join(".config").join("systemd").join("user");
        let log_dir = tmp.path().join("logs");
        let adapter = SystemdUserAdapter::new(&units_dir);

        let initial = adapter.status();
        assert_eq!(initial.platform, PLATFORM_NAME);
        assert_eq!(initial.unit_path, units_dir.join(DEFAULT_UNIT_NAME));
        assert!(!initial.installed);

        let spec = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::from("/usr/local/bin/switchboard"),
            config_path: Some(tmp.path().join("switchboard config.toml")),
            log_dir: log_dir.clone(),
        };

        let installed = adapter.install(&spec).unwrap();
        assert!(installed.installed);
        assert_eq!(installed.unit_path, units_dir.join(DEFAULT_UNIT_NAME));
        assert!(log_dir.is_dir());
        assert_eq!(
            fs::read_to_string(&installed.unit_path).unwrap(),
            adapter.render(&spec)
        );

        // Verify atomic write left no stray temp files behind.
        let entries: Vec<_> = fs::read_dir(&units_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);

        assert!(adapter.status().installed);

        let removed = adapter.uninstall().unwrap();
        assert!(!removed.installed);
        assert!(!removed.unit_path.exists());
        assert!(!adapter.status().installed);

        // Uninstalling a second time is idempotent.
        let removed_again = adapter.uninstall().unwrap();
        assert!(!removed_again.installed);
    }

    #[test]
    fn install_rejects_invalid_spec_paths() {
        let tmp = tempdir().unwrap();
        let adapter = SystemdUserAdapter::new(tmp.path());
        let spec = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::from("/usr/local/bin/switchboard"),
            config_path: Some(PathBuf::from("/tmp/bad\nconfig.toml")),
            log_dir: tmp.path().join("logs"),
        };

        assert!(matches!(
            adapter.install(&spec),
            Err(ServiceError::InvalidPath(_))
        ));
    }
}
