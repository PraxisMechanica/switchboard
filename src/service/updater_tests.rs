use std::process::Command;

use tempfile::tempdir;

use super::*;

#[test]
fn parses_sha256sums_lines_accurately() {
    let h1 = "a".repeat(64);
    let h2 = "b".repeat(64);
    let raw = format!(
        "{h1}  switchboard-v0.1.0-aarch64-apple-darwin.tar.gz\n{h2} *switchboard-v0.1.0-x86_64-unknown-linux-gnu.tar.gz\n"
    );
    assert_eq!(
        parse_sha256sums(&raw, "switchboard-v0.1.0-aarch64-apple-darwin.tar.gz").as_deref(),
        Some(h1.as_str())
    );
    assert_eq!(
        parse_sha256sums(&raw, "switchboard-v0.1.0-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
        Some(h2.as_str())
    );
    assert!(parse_sha256sums(&raw, "missing.tar.gz").is_none());
}

#[test]
fn detects_cargo_dev_build_paths() {
    assert!(is_dev_build_path(Path::new(
        "/Users/dev/switchboard/target/debug/switchboard"
    )));
    assert!(is_dev_build_path(Path::new(
        "/Users/dev/switchboard/target/release/switchboard"
    )));
    assert!(!is_dev_build_path(Path::new(
        "/Users/dev/.local/bin/switchboard"
    )));
}

#[tokio::test]
async fn heartbeat_updates_on_hash_change_and_skips_when_hash_matches() {
    let Some(target) = current_target_triple() else {
        return;
    };
    let dir = tempdir().unwrap();
    let release_dir = dir.path().join("release");
    let staging_dir = dir.path().join("staging");
    let data_dir = dir.path().join("data");
    let install_bin = dir.path().join("bin").join("switchboard");
    fs::create_dir_all(&release_dir).unwrap();
    fs::create_dir_all(&staging_dir).unwrap();
    fs::create_dir_all(install_bin.parent().unwrap()).unwrap();
    fs::write(&install_bin, b"old-binary").unwrap();

    fs::write(staging_dir.join("switchboard"), b"new-binary-v2").unwrap();
    let archive_name = format!("switchboard-{DEFAULT_RELEASE_TAG}-{target}.tar.gz");
    let archive_path = release_dir.join(&archive_name);
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging_dir)
        .arg("switchboard")
        .status()
        .unwrap();
    assert!(status.success());

    let expected = sha256_file(&archive_path).unwrap();
    fs::write(
        release_dir.join("SHA256SUMS.txt"),
        format!("{expected}  {archive_name}\n"),
    )
    .unwrap();

    let cfg = UpdateConfig {
        enabled: true,
        base_url: format!("file://{}", release_dir.display()),
        interval_secs: 600,
    };

    // First heartbeat: hash differs -> downloads, verifies SHA256, replaces binary atomically.
    let first = check_and_apply_update(&cfg, &data_dir, &install_bin, false)
        .await
        .unwrap();
    assert_eq!(
        first,
        UpdateOutcome::Updated {
            previous: None,
            current: expected.clone(),
        }
    );
    assert_eq!(fs::read(&install_bin).unwrap(), b"new-binary-v2");

    // Second heartbeat: hash matches -> no download, UpToDate.
    let second = check_and_apply_update(&cfg, &data_dir, &install_bin, false)
        .await
        .unwrap();
    assert_eq!(second, UpdateOutcome::UpToDate { hash: expected });
}
