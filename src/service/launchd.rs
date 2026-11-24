//! macOS `launchd` LaunchAgent plist generator and user-level service adapter.

use std::fmt::Write as _;
use std::path::PathBuf;

use super::{
    install_unit, uninstall_unit, unit_status, user_home_dir, ServiceError, ServiceSpec,
    ServiceStatus, ServiceUnitAdapter,
};

/// Platform identifier reported in [`ServiceStatus::platform`] for macOS LaunchAgents.
pub const PLATFORM_NAME: &str = "launchd";

/// Default LaunchAgent plist filename under `~/Library/LaunchAgents/`.
pub const DEFAULT_PLIST_NAME: &str = "com.praxis.switchboard.plist";

const STDOUT_LOG_FILE: &str = "switchboard.out.log";
const STDERR_LOG_FILE: &str = "switchboard.err.log";

/// User-level macOS `launchd` adapter managing a LaunchAgent `.plist` unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchdAdapter {
    base_dir: PathBuf,
    filename: String,
}

impl LaunchdAdapter {
    /// Constructs an adapter targeting `~/Library/LaunchAgents/com.praxis.switchboard.plist`.
    pub fn default_user() -> Result<Self, ServiceError> {
        let base_dir = user_home_dir()?.join("Library").join("LaunchAgents");
        Ok(Self::new(base_dir))
    }

    /// Constructs an adapter rooted in an explicit base directory (primarily for tests).
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filename: DEFAULT_PLIST_NAME.to_string(),
        }
    }

    /// Overrides the plist filename within `base_dir`.
    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = filename.into();
        self
    }
}

impl ServiceUnitAdapter for LaunchdAdapter {
    fn platform_name(&self) -> &'static str {
        PLATFORM_NAME
    }

    fn unit_path(&self) -> PathBuf {
        self.base_dir.join(&self.filename)
    }

    fn render(&self, spec: &ServiceSpec) -> String {
        let binary = spec.binary_path.to_string_lossy();
        let stdout_path = spec.log_dir.join(STDOUT_LOG_FILE);
        let stderr_path = spec.log_dir.join(STDERR_LOG_FILE);
        let stdout_str = stdout_path.to_string_lossy();
        let stderr_str = stderr_path.to_string_lossy();

        let mut out = String::new();
        let _ = writeln!(out, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        let _ = writeln!(
            out,
            r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#
        );
        let _ = writeln!(out, r#"<plist version="1.0">"#);
        let _ = writeln!(out, "<dict>");
        let _ = writeln!(out, "    <key>Label</key>");
        let _ = writeln!(out, "    <string>{}</string>", xml_escape(&spec.label));
        let _ = writeln!(out, "    <key>ProgramArguments</key>");
        let _ = writeln!(out, "    <array>");
        let _ = writeln!(out, "        <string>{}</string>", xml_escape(&binary));
        if let Some(config_path) = &spec.config_path {
            let cfg = config_path.to_string_lossy();
            let _ = writeln!(out, "        <string>--config</string>");
            let _ = writeln!(out, "        <string>{}</string>", xml_escape(&cfg));
        }
        let _ = writeln!(out, "        <string>daemon</string>");
        let _ = writeln!(out, "    </array>");
        let _ = writeln!(out, "    <key>RunAtLoad</key>");
        let _ = writeln!(out, "    <true/>");
        let _ = writeln!(out, "    <key>KeepAlive</key>");
        let _ = writeln!(out, "    <true/>");
        let _ = writeln!(out, "    <key>StandardOutPath</key>");
        let _ = writeln!(out, "    <string>{}</string>", xml_escape(&stdout_str));
        let _ = writeln!(out, "    <key>StandardErrorPath</key>");
        let _ = writeln!(out, "    <string>{}</string>", xml_escape(&stderr_str));
        let _ = writeln!(out, "</dict>");
        let _ = writeln!(out, "</plist>");
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

/// Escapes XML 1.0 reserved characters (`&`, `<`, `>`, `"`, `'`).
pub fn xml_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;
    use crate::service::DEFAULT_SERVICE_LABEL;

    #[test]
    fn xml_escape_escapes_all_reserved_characters() {
        assert_eq!(
            xml_escape(r#"a & b < c > d "e" 'f'"#),
            "a &amp; b &lt; c &gt; d &quot;e&quot; &apos;f&apos;"
        );
        assert_eq!(
            xml_escape("/usr/local/bin/switchboard"),
            "/usr/local/bin/switchboard"
        );
    }

    #[test]
    fn render_without_config_sets_exact_arguments_and_keys() {
        let adapter = LaunchdAdapter::new("/tmp/launch-agents");
        let spec = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::from("/usr/local/bin/switchboard"),
            config_path: None,
            log_dir: PathBuf::from("/Users/alice/.local/share/switchboard/logs"),
        };

        let rendered = adapter.render(&spec);
        let expected = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n",
            "<dict>\n",
            "    <key>Label</key>\n",
            "    <string>com.praxis.switchboard</string>\n",
            "    <key>ProgramArguments</key>\n",
            "    <array>\n",
            "        <string>/usr/local/bin/switchboard</string>\n",
            "        <string>daemon</string>\n",
            "    </array>\n",
            "    <key>RunAtLoad</key>\n",
            "    <true/>\n",
            "    <key>KeepAlive</key>\n",
            "    <true/>\n",
            "    <key>StandardOutPath</key>\n",
            "    <string>/Users/alice/.local/share/switchboard/logs/switchboard.out.log</string>\n",
            "    <key>StandardErrorPath</key>\n",
            "    <string>/Users/alice/.local/share/switchboard/logs/switchboard.err.log</string>\n",
            "</dict>\n",
            "</plist>\n",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_with_config_and_special_characters_escapes_xml() {
        let adapter = LaunchdAdapter::new("/tmp/launch-agents");
        let spec = ServiceSpec {
            label: "com.praxis.a&b<1>".to_string(),
            binary_path: PathBuf::from(r#"/opt/A&B's "Apps"/switchboard"#),
            config_path: Some(PathBuf::from("/Users/a<b/config & 'test'.toml")),
            log_dir: PathBuf::from("/tmp/logs & <err>"),
        };

        let rendered = adapter.render(&spec);
        assert!(rendered.contains("<string>com.praxis.a&amp;b&lt;1&gt;</string>"));
        assert!(rendered.contains(
            "        <string>/opt/A&amp;B&apos;s &quot;Apps&quot;/switchboard</string>\n        <string>--config</string>\n        <string>/Users/a&lt;b/config &amp; &apos;test&apos;.toml</string>\n        <string>daemon</string>"
        ));
        assert!(
            rendered.contains("<string>/tmp/logs &amp; &lt;err&gt;/switchboard.out.log</string>")
        );
        assert!(
            rendered.contains("<string>/tmp/logs &amp; &lt;err&gt;/switchboard.err.log</string>")
        );
    }

    #[test]
    fn install_status_and_uninstall_lifecycle_in_tempdir() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("Library").join("LaunchAgents");
        let log_dir = tmp.path().join("logs").join("switchboard");
        let adapter = LaunchdAdapter::new(&agents_dir);

        let initial = adapter.status();
        assert_eq!(initial.platform, PLATFORM_NAME);
        assert_eq!(initial.unit_path, agents_dir.join(DEFAULT_PLIST_NAME));
        assert!(!initial.installed);

        let spec = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::from("/usr/local/bin/switchboard"),
            config_path: Some(tmp.path().join("config.toml")),
            log_dir: log_dir.clone(),
        };

        let installed = adapter.install(&spec).unwrap();
        assert!(installed.installed);
        assert_eq!(installed.unit_path, agents_dir.join(DEFAULT_PLIST_NAME));
        assert!(log_dir.is_dir());
        assert_eq!(
            fs::read_to_string(&installed.unit_path).unwrap(),
            adapter.render(&spec)
        );

        // Verify atomic write left no stray temp files behind.
        let entries: Vec<_> = fs::read_dir(&agents_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);

        let after_install = adapter.status();
        assert!(after_install.installed);

        let removed = adapter.uninstall().unwrap();
        assert!(!removed.installed);
        assert!(!removed.unit_path.exists());
        assert!(!adapter.status().installed);

        // Uninstalling a second time is idempotent.
        let removed_again = adapter.uninstall().unwrap();
        assert!(!removed_again.installed);
    }

    #[test]
    fn install_rejects_empty_binary_path() {
        let tmp = tempdir().unwrap();
        let adapter = LaunchdAdapter::new(tmp.path());
        let spec = ServiceSpec {
            label: DEFAULT_SERVICE_LABEL.to_string(),
            binary_path: PathBuf::new(),
            config_path: None,
            log_dir: tmp.path().join("logs"),
        };

        assert!(matches!(
            adapter.install(&spec),
            Err(ServiceError::InvalidPath(_))
        ));
    }
}
