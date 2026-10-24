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

fn make_fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    // Safety: name is a valid NUL-terminated path; mkfifo does not retain it.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

#[test]
fn fifo_child_probe() {
    let Some(root) = std::env::var_os("LIBREPODS_FIFO_TEST_ROOT") else {
        return;
    };
    let paths = paths(Path::new(&root));
    match std::env::var("LIBREPODS_FIFO_TEST_OPERATION")
        .expect("probe operation")
        .as_str()
    {
        "config" => {
            assert!(paths.load_config().is_err(), "FIFO config was accepted");
        }
        "owner-cleanup" => {
            drop(OwnerGuard::acquire(&paths, "probe").expect("owner"));
        }
        "legacy-shutdown" => {
            tokio::runtime::Runtime::new()
                .expect("child runtime")
                .block_on(async move {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    let clone = paths.clone();
                    let server = tokio::spawn(librepods_daemon::runtime::run(
                        clone,
                        Config {
                            legacy_compatibility: true,
                            ..Config::default()
                        },
                        std::sync::Arc::new(NoDevice),
                        async {
                            let _ = receiver.await;
                        },
                    ));
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                    while !paths.socket().exists() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "child service did not start"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    sender.send(()).expect("shutdown request");
                    server.await.expect("service join").expect("service result");
                    use std::os::unix::fs::FileTypeExt;
                    assert!(paths
                        .legacy_state
                        .symlink_metadata()
                        .expect("FIFO preserved")
                        .file_type()
                        .is_fifo());
                });
        }
        other => panic!("unexpected probe operation {other}"),
    }
}

#[test]
fn fifo_config_and_owner_cleanup_cannot_block_before_regular_file_validation() {
    for operation in ["config", "owner-cleanup", "legacy-shutdown"] {
        let directory = tempfile::tempdir().expect("test directory");
        let paths = paths(directory.path());
        fs::create_dir_all(paths.config.parent().expect("config parent"))
            .expect("config directory");
        fs::create_dir_all(&paths.state).expect("state directory");
        fs::set_permissions(&paths.state, fs::Permissions::from_mode(0o700))
            .expect("private directory");
        fs::create_dir_all(paths.legacy_state.parent().expect("legacy parent"))
            .expect("legacy directory");
        fs::set_permissions(
            paths.legacy_state.parent().expect("legacy parent"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("private legacy parent");
        let destination = match operation {
            "config" => paths.config.clone(),
            "owner-cleanup" => paths.state_file(),
            _ => paths.legacy_state.clone(),
        };
        make_fifo(&destination);
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "fifo_child_probe", "--nocapture"])
                .env("LIBREPODS_FIFO_TEST_ROOT", directory.path())
                .env("LIBREPODS_FIFO_TEST_OPERATION", operation)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("bounded child probe");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if child.try_wait().expect("child status").is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().expect("terminate blocked FIFO probe");
                child.wait().expect("reap probe");
                panic!("{operation} blocked opening a FIFO before validating regular file type");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let result = child.wait_with_output().expect("child output");
        assert!(
            result.status.success(),
            "{operation}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

struct NoDevice;
impl librepods_daemon::backend::Backend for NoDevice {
    fn discover(
        &self,
        _: Option<&str>,
    ) -> Result<librepods_daemon::backend::Endpoint, librepods_daemon::DaemonError> {
        Err(librepods_daemon::DaemonError::transient(
            ErrorCode::AdapterUnavailable,
            "fake discovery",
            "no adapter",
        ))
    }
    fn connect(
        &self,
        _: &librepods_daemon::backend::Endpoint,
        _: std::time::Duration,
    ) -> Result<
        std::sync::Arc<dyn librepods_daemon::backend::SessionTransport>,
        librepods_daemon::DaemonError,
    > {
        Err(librepods_daemon::DaemonError::new(
            ErrorCode::ConnectFailed,
            "fake connect",
            "no endpoint",
        ))
    }
}
