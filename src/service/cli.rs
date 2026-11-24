//! CLI definitions for the daemon service vertical slice.

use std::path::PathBuf;

use clap::Subcommand;

use super::{default_adapter_for_host, ServiceError, ServiceSpec, DEFAULT_SERVICE_LABEL};

/// Subcommands for `switchboard service`.
#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// Install the user-level background daemon service unit.
    Install {
        /// Optional path to the `switchboard` binary (defaults to the current executable).
        #[arg(long)]
        binary: Option<PathBuf>,
    },

    /// Remove the installed user-level background daemon service unit.
    Uninstall,

    /// Inspect whether the user-level background daemon service unit is installed.
    Status {
        /// Output machine-readable JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// Executes a `switchboard service` subcommand against the host OS adapter.
pub fn run_service_cmd(
    cmd: ServiceCmd,
    config_path: Option<PathBuf>,
    log_dir: PathBuf,
) -> Result<(), ServiceError> {
    let adapter = default_adapter_for_host()?;
    match cmd {
        ServiceCmd::Install { binary } => {
            let binary_path = match binary {
                Some(p) => p,
                None => std::env::current_exe()?,
            };
            let spec = ServiceSpec {
                label: DEFAULT_SERVICE_LABEL.to_string(),
                binary_path,
                config_path,
                log_dir,
            };
            let st = adapter.install(&spec)?;
            println!(
                "Installed {} service unit at {}",
                st.platform,
                st.unit_path.display()
            );
        }
        ServiceCmd::Uninstall => {
            let st = adapter.uninstall()?;
            println!(
                "Removed {} service unit at {}",
                st.platform,
                st.unit_path.display()
            );
        }
        ServiceCmd::Status { json } => {
            let st = adapter.status();
            if json {
                if let Ok(encoded) = serde_json::to_string_pretty(&st) {
                    println!("{encoded}");
                }
            } else {
                println!(
                    "platform:  {}\nunit_path: {}\ninstalled: {}",
                    st.platform,
                    st.unit_path.display(),
                    st.installed
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::ServiceCmd;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: ServiceCmd,
    }

    #[test]
    fn parses_service_subcommands() {
        let install_default = TestCli::try_parse_from(["service", "install"]).unwrap();
        assert!(matches!(
            install_default.cmd,
            ServiceCmd::Install { binary: None }
        ));

        let install_explicit =
            TestCli::try_parse_from(["service", "install", "--binary", "/usr/local/bin/sb"])
                .unwrap();
        match install_explicit.cmd {
            ServiceCmd::Install { binary } => {
                assert_eq!(binary, Some(PathBuf::from("/usr/local/bin/sb")));
            }
            other => panic!("expected Install, got {other:?}"),
        }

        let uninstall = TestCli::try_parse_from(["service", "uninstall"]).unwrap();
        assert!(matches!(uninstall.cmd, ServiceCmd::Uninstall));

        let status_plain = TestCli::try_parse_from(["service", "status"]).unwrap();
        assert!(matches!(
            status_plain.cmd,
            ServiceCmd::Status { json: false }
        ));

        let status_json = TestCli::try_parse_from(["service", "status", "--json"]).unwrap();
        assert!(matches!(status_json.cmd, ServiceCmd::Status { json: true }));
    }
}
