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

#[test]
fn inherited_lock_child_probe() {
    use std::io::Read;
    let Some(fd) = std::env::var_os("LIBREPODS_INHERITED_LOCK_FD") else {
        return;
    };
    let path = std::path::PathBuf::from("/proc/self/fd").join(fd);
    assert!(path
        .metadata()
        .expect("inherited lock descriptor")
        .is_file());
    std::io::stdin()
        .read_to_end(&mut Vec::new())
        .expect("parent lifetime pipe");
}

#[test]
fn owner_drop_releases_lock_even_while_an_exec_child_inherits_the_open_file_description() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let directory = tempfile::tempdir().expect("test directory");
    let paths = paths(directory.path());
    let guard = OwnerGuard::acquire(&paths, "first").expect("owner");
    let lock_path = paths.runtime.join("owner.lock");
    let fd = fs::read_dir("/proc/self/fd")
        .expect("descriptor directory")
        .filter_map(Result::ok)
        .find(|entry| fs::read_link(entry.path()).is_ok_and(|path| path == lock_path))
        .expect("owner lock descriptor")
        .file_name()
        .to_str()
        .expect("numeric fd")
        .parse::<i32>()
        .expect("descriptor number");
    // Safety: fd was found in this process; F_DUPFD creates a new owned descriptor
    // without CLOEXEC. No Rust code or allocation runs in a post-fork child hook.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD, 3) };
    assert!(duplicate >= 0, "duplicate lock descriptor");
    // Safety: F_DUPFD returned a new descriptor transferred to this OwnedFd.
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "inherited_lock_child_probe", "--nocapture"])
        .env(
            "LIBREPODS_INHERITED_LOCK_FD",
            duplicate.as_raw_fd().to_string(),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("child holding inherited OFD");
    drop(duplicate);
    drop(guard);
    let replacement = OwnerGuard::acquire(&paths, "replacement");
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while child.try_wait().expect("child status").is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().expect("terminate child");
            child.wait().expect("reap child");
            panic!("inherited descriptor child did not stop");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        replacement.is_ok(),
        "explicit owner drop left flock held by an inherited descriptor"
    );
}

#[test]
fn file_size_limit_child_probe() {
    let Some(root) = std::env::var_os("LIBREPODS_WRITE_FAILURE_ROOT") else {
        return;
    };
    let paths = paths(Path::new(&root));
    let original = Snapshot::new("last-good-owner".into(), &Default::default());
    atomic_write(
        &paths.state_file(),
        &serde_json::to_vec(&original).expect("old snapshot"),
    )
    .expect("publish last good snapshot before limit");
    let old_bytes = fs::read(paths.state_file()).expect("last good bytes");
    // This child alone ignores SIGXFSZ and lowers its own file-size limit. A
    // partial regular-file write succeeds before the next write reports EFBIG.
    // No filesystem is filled and no parent/global resource limit changes.
    let mut original_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // Safety: output storage is initialized and valid for getrlimit.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut original_limit) },
        0
    );
    let limit = libc::rlimit {
        rlim_cur: 128,
        rlim_max: original_limit.rlim_max,
    };
    // Safety: SIG_IGN is a valid signal disposition; limit points to initialized
    // data. Both changes affect only this expendable child process.
    let original_signal = unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    assert_ne!(original_signal, libc::SIG_ERR);
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);
    let result = atomic_write(&paths.state_file(), &vec![b'x'; 65536]);
    // Restore the child soft limit before assertions/exit so LLVM can flush its
    // complete coverage profile rather than emitting a truncated profraw file.
    // Safety: the original hard limit was retained, so raising the soft limit
    // back to its original value requires no privilege or parent modification.
    unsafe {
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &original_limit), 0);
        assert_ne!(libc::signal(libc::SIGXFSZ, original_signal), libc::SIG_ERR);
    }
    let error = result.expect_err("mid-write file-size failure");
    assert_eq!(error.stage, "publish write");
    assert_eq!(
        error.message,
        std::io::Error::from_raw_os_error(libc::EFBIG).to_string()
    );
    assert_eq!(
        fs::read(paths.state_file()).expect("old status remains readable"),
        old_bytes
    );
    let names: Vec<_> = fs::read_dir(&paths.state)
        .expect("state directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(
        names,
        vec![std::ffi::OsString::from("status.json")],
        "mid-write failure leaked an atomic publication temporary"
    );
}

#[test]
fn actual_mid_write_failure_preserves_last_good_status_and_removes_partial_temporary() {
    let directory = tempfile::tempdir().expect("private test directory");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "file_size_limit_child_probe",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LIBREPODS_WRITE_FAILURE_ROOT", directory.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("isolated resource-limit child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while child.try_wait().expect("child status").is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().expect("terminate stalled write probe");
            child.wait().expect("reap probe");
            panic!("mid-write fault probe exceeded its bound");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let output = child.wait_with_output().expect("child output");
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn synthetic_disk_full_error_retains_os_cause_without_filling_a_filesystem() {
    let cause = std::io::Error::from_raw_os_error(libc::ENOSPC);
    let expected = cause.to_string();
    let classified = librepods_daemon::DaemonError::io("publish write", cause);
    assert_eq!(classified.stage, "publish write");
    assert_eq!(classified.message, expected);
    let public: librepods_daemon::DaemonError =
        serde_json::from_slice(&serde_json::to_vec(&classified).expect("public error JSON"))
            .expect("decoded error");
    assert_eq!(
        public.message, expected,
        "structured error discarded the disk-full cause"
    );
}

#[test]
fn owner_refuses_a_live_unlocked_socket_and_reclaims_only_a_dead_socket() {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(directory.path());
    fs::create_dir_all(&paths.runtime).unwrap();
    fs::set_permissions(&paths.runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(paths.socket()).unwrap();
    let original_inode = fs::metadata(paths.socket()).unwrap().ino();
    let error = match OwnerGuard::acquire(&paths, "new-owner") {
        Ok(_) => panic!("an unlocked live listener must still prevent takeover"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::ServiceUnavailable);
    assert_eq!(fs::metadata(paths.socket()).unwrap().ino(), original_inode);
    // Shut down the socket itself before dropping the descriptor: parallel
    // process fixtures can briefly inherit a CLOEXEC fd before exec closes it.
    // Safety: listener owns a valid Unix socket descriptor throughout this call.
    assert_eq!(
        unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) },
        0
    );
    drop(listener);
    let mut guard = OwnerGuard::acquire(&paths, "replacement").unwrap();
    assert!(!paths.socket().exists(), "dead socket was not reclaimed");
    // Recording an unrelated replacement must fail and must not grant cleanup
    // ownership of that regular file.
    fs::write(paths.socket(), b"foreign replacement").unwrap();
    assert_eq!(
        guard.record_socket().unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    drop(guard);
    assert_eq!(fs::read(paths.socket()).unwrap(), b"foreign replacement");
}

#[test]
fn invalid_existing_publications_and_legacy_owner_are_preserved() {
    use librepods_daemon::storage::{publish_legacy, read_private_json};
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(directory.path());
    fs::create_dir_all(paths.legacy_state.parent().unwrap()).unwrap();
    for bytes in [
        b"broken JSON".to_vec(),
        vec![b' '; librepods_daemon::ipc::MAX_RESPONSE_FRAME + 1],
    ] {
        atomic_write(&paths.legacy_state, &bytes).unwrap();
        assert_eq!(
            read_private_json(&paths.legacy_state).unwrap_err().code,
            ErrorCode::InvalidInput
        );
        assert!(publish_legacy(&paths, "new", b"{}").is_err());
        assert_eq!(fs::read(&paths.legacy_state).unwrap(), bytes);
    }
    fs::remove_file(&paths.legacy_state).unwrap();
    fs::write(&paths.legacy_socket, b"foreign owner marker").unwrap();
    assert_eq!(
        publish_legacy(&paths, "new", b"{}").unwrap_err().code,
        ErrorCode::ServiceUnavailable
    );
    assert!(!paths.legacy_state.exists());
    assert_eq!(
        fs::read(&paths.legacy_socket).unwrap(),
        b"foreign owner marker"
    );
}
