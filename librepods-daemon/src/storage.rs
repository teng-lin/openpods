// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{
    config::{self, Config},
    snapshot::Snapshot,
    DaemonError, ErrorCode,
};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Paths {
    pub runtime: PathBuf,
    pub state: PathBuf,
    pub config: PathBuf,
    pub legacy_state: PathBuf,
    pub legacy_socket: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self, DaemonError> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
            DaemonError::new(
                ErrorCode::InvalidInput,
                "runtime",
                "XDG_RUNTIME_DIR is required",
            )
        })?;
        let home = std::env::var_os("HOME").ok_or_else(|| {
            DaemonError::new(ErrorCode::InvalidInput, "runtime", "HOME is required")
        })?;
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".local/state"));
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".config"));
        let runtime = PathBuf::from(runtime);
        if !runtime.is_absolute() || !state.is_absolute() || !config.is_absolute() {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "runtime",
                "XDG directories must be absolute",
            ));
        }
        validate_directory(&runtime)?;
        Ok(Self {
            runtime: runtime.join("librepods-rs"),
            state: state.join("librepods-rs"),
            config: config.join("librepods-rs/config.json"),
            legacy_state: state.join("librepods/status.json"),
            legacy_socket: runtime.join("librepods.sock"),
        })
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime.join("control.sock")
    }
    pub fn state_file(&self) -> PathBuf {
        self.state.join("status.json")
    }
    pub fn load_config(&self) -> Result<Config, DaemonError> {
        match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.config)
        {
            Ok(file) => {
                validate_file(&file)?;
                let mut bytes = Vec::new();
                file.take(16 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| DaemonError::io("config read", e))?;
                config::parse(&bytes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(DaemonError::io("config open", e)),
        }
    }
    pub fn save_config(&self, config: &Config) -> Result<(), DaemonError> {
        config.validate()?;
        let bytes = serde_json::to_vec_pretty(config)
            .map_err(|e| DaemonError::new(ErrorCode::Internal, "config", e.to_string()))?;
        atomic_write(&self.config, &bytes)
    }
}

fn uid() -> u32 {
    // Safety: getuid has no preconditions or mutable process state.
    unsafe { libc::getuid() }
}

fn validate_directory(path: &Path) -> Result<(), DaemonError> {
    let metadata = fs::symlink_metadata(path).map_err(|e| DaemonError::io("directory", e))?;
    if !metadata.is_dir() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        return Err(DaemonError::new(
            ErrorCode::PermissionDenied,
            "directory",
            "directory must be an owned, private, real directory",
        ));
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), DaemonError> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| DaemonError::io("create directory", e))?;
    }
    validate_directory(path)
}

fn validate_file(file: &File) -> Result<(), DaemonError> {
    let metadata = file
        .metadata()
        .map_err(|e| DaemonError::io("file metadata", e))?;
    if !metadata.is_file() || metadata.uid() != uid() || metadata.mode() & 0o077 != 0 {
        return Err(DaemonError::new(
            ErrorCode::PermissionDenied,
            "file",
            "file must be owned and private",
        ));
    }
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), DaemonError> {
    let directory = path.parent().ok_or_else(|| {
        DaemonError::new(
            ErrorCode::InvalidInput,
            "publish",
            "missing parent directory",
        )
    })?;
    private_directory(directory)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".write-{}-{sequence}", std::process::id()));
    let mut created = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| DaemonError::io("publish temporary", e))?;
        created = true;
        file.write_all(bytes)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|e| DaemonError::io("publish write", e))?;
        fs::rename(&temporary, path).map_err(|e| DaemonError::io("publish rename", e))?;
        File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| DaemonError::io("publish sync", e))?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub struct OwnerGuard {
    _lock: File,
    socket: PathBuf,
    state: PathBuf,
    instance: String,
    socket_identity: Option<(u64, u64)>,
}

impl OwnerGuard {
    pub fn acquire(paths: &Paths, instance: &str) -> Result<Self, DaemonError> {
        private_directory(&paths.runtime)?;
        private_directory(&paths.state)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(paths.runtime.join("owner.lock"))
            .map_err(|e| DaemonError::io("owner lock", e))?;
        validate_file(&lock)?;
        // Safety: lock is an owned open descriptor; flock keeps the lock until drop.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(DaemonError::new(
                ErrorCode::ServiceUnavailable,
                "owner lock",
                "another daemon owns the runtime directory",
            ));
        }
        let socket = paths.socket();
        if let Ok(metadata) = fs::symlink_metadata(&socket) {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() || metadata.uid() != uid() {
                return Err(DaemonError::new(
                    ErrorCode::PermissionDenied,
                    "socket",
                    "unexpected file occupies the socket path",
                ));
            }
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return Err(DaemonError::new(
                    ErrorCode::ServiceUnavailable,
                    "socket",
                    "another service is listening",
                ));
            }
            fs::remove_file(&socket).map_err(|e| DaemonError::io("stale socket", e))?;
        }
        Ok(Self {
            _lock: lock,
            socket,
            state: paths.state_file(),
            instance: instance.into(),
            socket_identity: None,
        })
    }

    pub fn record_socket(&mut self) -> Result<(), DaemonError> {
        use std::os::unix::fs::FileTypeExt;
        let metadata = fs::symlink_metadata(&self.socket)
            .map_err(|e| DaemonError::io("socket identity", e))?;
        if !metadata.file_type().is_socket() || metadata.uid() != uid() {
            return Err(DaemonError::new(
                ErrorCode::PermissionDenied,
                "socket identity",
                "unexpected socket owner or type",
            ));
        }
        self.socket_identity = Some((metadata.dev(), metadata.ino()));
        Ok(())
    }
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        if let (Some(identity), Ok(metadata)) =
            (self.socket_identity, fs::symlink_metadata(&self.socket))
        {
            if (metadata.dev(), metadata.ino()) == identity {
                let _ = fs::remove_file(&self.socket);
            }
        }
        if let Ok(bytes) = fs::read(&self.state) {
            if serde_json::from_slice::<Snapshot>(&bytes)
                .is_ok_and(|s| s.daemon_instance == self.instance)
            {
                let _ = fs::remove_file(&self.state);
            }
        }
    }
}

pub fn restrict_socket(path: &Path) -> Result<(), DaemonError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| DaemonError::io("socket permissions", e))
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn single_owner_private_atomic_status_and_cleanup() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(temp.path());
        let guard = OwnerGuard::acquire(&paths, "one").expect("owner");
        assert!(OwnerGuard::acquire(&paths, "two").is_err());
        let status = Snapshot::new("one".into(), &Default::default());
        atomic_write(
            &paths.state_file(),
            &serde_json::to_vec(&status).expect("json"),
        )
        .expect("publish");
        assert_eq!(
            fs::metadata(paths.state_file())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(guard);
        assert!(!paths.state_file().exists());
    }
    #[test]
    fn preserves_unexpected_socket_path_and_future_config() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(temp.path());
        private_directory(&paths.runtime).expect("run");
        fs::write(paths.socket(), b"user data").expect("write");
        assert!(OwnerGuard::acquire(&paths, "one").is_err());
        assert_eq!(fs::read(paths.socket()).expect("read"), b"user data");
        atomic_write(&paths.config, br#"{"schema_version":2}"#).expect("config");
        assert!(paths.load_config().is_err());
    }
    #[test]
    fn failed_publication_does_not_destroy_previous_status() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("private/status.json");
        atomic_write(&path, b"old").expect("old");
        assert!(atomic_write(&path.join("invalid"), b"new").is_err());
        assert_eq!(fs::read(&path).expect("old remains"), b"old\n");
    }

    #[test]
    fn cleanup_does_not_remove_an_unexpected_replacement_file() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(temp.path());
        let guard = OwnerGuard::acquire(&paths, "one").expect("owner");
        fs::write(paths.socket(), b"replacement").expect("replace");
        drop(guard);
        assert_eq!(
            fs::read(paths.socket()).expect("replacement survives"),
            b"replacement"
        );
    }
}
