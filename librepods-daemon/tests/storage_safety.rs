// SPDX-License-Identifier: AGPL-3.0-only
#![cfg(target_os = "linux")]
use librepods_daemon::{
    config::Config,
    snapshot::Snapshot,
    storage::{atomic_write, OwnerGuard, Paths},
    ErrorCode,
};
use std::{
    fs,
    os::unix::{
        fs::{symlink, PermissionsExt},
        net::UnixListener,
    },
    path::Path,
};

fn paths(root: &Path) -> Paths {
    Paths {
        runtime: root.join("run"),
        state: root.join("state"),
        config: root.join("config/config.json"),
        legacy_state: root.join("legacy/status.json"),
        legacy_socket: root.join("legacy.sock"),
    }
}

#[test]
fn cleanup_preserves_another_instances_snapshot_and_rejects_symlink_replacements() {
    let directory = tempfile::tempdir().expect("private test directory");
    let paths = paths(directory.path());
    let mut guard = OwnerGuard::acquire(&paths, "original").expect("owner");
    let listener = UnixListener::bind(paths.socket()).expect("listener");
    guard.record_socket().expect("socket ownership");
    let foreign = Snapshot::new("other-instance".into(), &Default::default());
    atomic_write(
        &paths.state_file(),
        &serde_json::to_vec(&foreign).expect("snapshot"),
    )
    .expect("foreign status");
    drop(guard);
    assert!(!paths.socket().exists());
    assert_eq!(
        serde_json::from_slice::<Snapshot>(
            &fs::read(paths.state_file()).expect("preserved status")
        )
        .expect("snapshot")
        .daemon_instance,
        "other-instance"
    );
    drop(listener);
    let guard = OwnerGuard::acquire(&paths, "other-instance").expect("new owner");
    let unrelated = directory.path().join("unrelated.json");
    fs::rename(paths.state_file(), &unrelated).expect("move foreign data");
    symlink(&unrelated, paths.state_file()).expect("symlink replacement");
    drop(guard);
    assert!(paths
        .state_file()
        .symlink_metadata()
        .expect("symlink preserved")
        .file_type()
        .is_symlink());
    assert!(unrelated.is_file(), "unrelated target was changed");
}

#[test]
fn publication_failure_removes_only_its_temporary_and_keeps_other_files() {
    let directory = tempfile::tempdir().expect("test directory");
    let destination = directory.path().join("status.json");
    fs::create_dir(&destination).expect("rename obstacle");
    let unrelated = directory.path().join("keep");
    fs::write(&unrelated, b"keep").expect("unrelated file");
    assert!(atomic_write(&destination, b"new status").is_err());
    let names: Vec<_> = fs::read_dir(directory.path())
        .expect("directory")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names.len(), 2, "failed publication leaked a temporary file");
    assert_eq!(fs::read(unrelated).expect("preserved file"), b"keep");
    assert!(destination.is_dir());
}

#[test]
fn config_requires_private_regular_bounded_files_and_never_follows_a_symlink() {
    let directory = tempfile::tempdir().expect("test directory");
    let paths = paths(directory.path());
    assert!(
        !paths
            .load_config()
            .expect("missing config defaults")
            .ble_enabled
    );
    paths.save_config(&Config::default()).expect("config save");
    assert_eq!(
        fs::metadata(&paths.config)
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o644)).expect("permissions");
    assert_eq!(
        paths.load_config().expect_err("public config").code,
        ErrorCode::PermissionDenied
    );
    fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o600)).expect("private mode");
    fs::write(&paths.config, vec![b' '; 16385]).expect("oversized config");
    assert_eq!(
        paths.load_config().expect_err("oversized config").code,
        ErrorCode::InvalidInput
    );
    fs::remove_file(&paths.config).expect("remove test config");
    let other = directory.path().join("other.json");
    fs::write(&other, b"{}").expect("other config");
    symlink(&other, &paths.config).expect("config symlink");
    assert!(paths.load_config().is_err());
    assert_eq!(fs::read(other).expect("unchanged symlink target"), b"{}");
}
