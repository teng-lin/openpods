// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Opt-in BLE keys, kept separate from public status and diagnostic DTOs.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{DaemonError, ErrorCode};

const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_DEVICES: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct BleKeys {
    address: String,
    irk: [u8; 16],
    encryption_key: [u8; 16],
}

impl std::fmt::Debug for BleKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BleKeys([redacted])")
    }
}

pub fn address_bytes(address: &str) -> Result<[u8; 6], DaemonError> {
    let mut components = address.split(':');
    let mut bytes = [0u8; 6];
    for byte in &mut bytes {
        let part = components.next().ok_or_else(invalid_address)?;
        if part.len() != 2 {
            return Err(invalid_address());
        }
        *byte = u8::from_str_radix(part, 16).map_err(|_| invalid_address())?;
    }
    if components.next().is_some() {
        return Err(invalid_address());
    }
    Ok(bytes)
}

fn invalid_address() -> DaemonError {
    DaemonError::new(
        ErrorCode::InvalidInput,
        "BLE keys",
        "invalid Bluetooth address",
    )
}

fn canonical_address(address: &str) -> Result<String, DaemonError> {
    let bytes = address_bytes(address)?;
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":"))
}

impl BleKeys {
    pub fn new(
        address: &str,
        irk: [u8; 16],
        encryption_key: [u8; 16],
    ) -> Result<Self, DaemonError> {
        Ok(Self {
            address: canonical_address(address)?,
            irk,
            encryption_key,
        })
    }
    pub fn address(&self) -> &str {
        &self.address
    }
    #[cfg(feature = "ble")]
    pub(crate) fn irk(&self) -> &[u8; 16] {
        &self.irk
    }
    #[cfg(feature = "ble")]
    pub(crate) fn encryption_key(&self) -> &[u8; 16] {
        &self.encryption_key
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredKeys {
    irk: [u8; 16],
    encryption_key: [u8; 16],
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    schema_version: u32,
    devices: BTreeMap<String, StoredKeys>,
}

pub struct KeyStore {
    path: PathBuf,
    data: KeyFile,
}

impl KeyStore {
    pub fn load(path: PathBuf) -> Result<Self, DaemonError> {
        let empty = || KeyFile {
            schema_version: 1,
            devices: BTreeMap::new(),
        };
        let data = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => {
                let metadata = file
                    .metadata()
                    .map_err(|error| DaemonError::io("BLE key metadata", error))?;
                // Safety: getuid has no preconditions and changes no process state.
                let uid = unsafe { libc::getuid() };
                if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
                    return Err(DaemonError::new(
                        ErrorCode::PermissionDenied,
                        "BLE keys",
                        "key store must be a private owned regular file",
                    ));
                }
                let mut bytes = Vec::new();
                file.take(MAX_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| DaemonError::io("BLE key read", error))?;
                if bytes.len() > MAX_FILE_BYTES {
                    return Err(DaemonError::new(
                        ErrorCode::InvalidInput,
                        "BLE keys",
                        "key store exceeds size limit",
                    ));
                }
                serde_json::from_slice::<KeyFile>(&bytes).map_err(|error| {
                    DaemonError::new(
                        ErrorCode::InvalidInput,
                        "BLE keys",
                        format!(
                            "invalid key store encoding at line {}, column {} ({:?})",
                            error.line(),
                            error.column(),
                            error.classify()
                        ),
                    )
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => empty(),
            Err(error) => return Err(DaemonError::io("BLE key open", error)),
        };
        if data.schema_version != 1 {
            return Err(DaemonError::new(
                ErrorCode::VersionMismatch,
                "BLE keys",
                "unsupported key store schema",
            ));
        }
        if data.devices.len() > MAX_DEVICES
            || data.devices.keys().any(|address| {
                !canonical_address(address).is_ok_and(|canonical| &canonical == address)
            })
        {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "BLE keys",
                "invalid stored device identities",
            ));
        }
        Ok(Self { path, data })
    }

    pub fn get(&self, address: &str) -> Result<Option<BleKeys>, DaemonError> {
        let address = canonical_address(address)?;
        Ok(self.data.devices.get(&address).map(|keys| BleKeys {
            address,
            irk: keys.irk,
            encryption_key: keys.encryption_key,
        }))
    }

    pub fn save(&mut self, keys: &BleKeys) -> Result<(), DaemonError> {
        let mut next = self.data.clone();
        next.devices.insert(
            keys.address.clone(),
            StoredKeys {
                irk: keys.irk,
                encryption_key: keys.encryption_key,
            },
        );
        self.commit(next)
    }

    pub fn remove(&mut self, address: &str) -> Result<(), DaemonError> {
        let mut next = self.data.clone();
        next.devices.remove(&canonical_address(address)?);
        self.commit(next)
    }

    fn commit(&mut self, next: KeyFile) -> Result<(), DaemonError> {
        let bytes = serde_json::to_vec(&next).map_err(|_| {
            DaemonError::new(ErrorCode::Internal, "BLE keys", "cannot encode key store")
        })?;
        if next.devices.len() > MAX_DEVICES || bytes.len().saturating_add(1) > MAX_FILE_BYTES {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "BLE keys",
                "key store capacity exceeded",
            ));
        }
        crate::storage::atomic_write(&self.path, &bytes)?;
        self.data = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_key_roundtrip_and_removal_never_expose_debug_values() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("keys/keys.json");
        let mut store = KeyStore::load(path.clone()).expect("empty store");
        let keys = BleKeys::new("aa:bb:cc:dd:ee:ff", [7; 16], [9; 16]).expect("keys");
        assert_eq!(format!("{keys:?}"), "BleKeys([redacted])");
        store.save(&keys).expect("save");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut reloaded = KeyStore::load(path).expect("reload");
        assert_eq!(reloaded.get("AA:BB:CC:DD:EE:FF").expect("get"), Some(keys));
        reloaded.remove("AA:BB:CC:DD:EE:FF").expect("forget");
        assert!(reloaded.get("AA:BB:CC:DD:EE:FF").expect("get").is_none());
    }

    #[test]
    fn unsafe_permissions_symlinks_and_newer_schemas_are_rejected() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("keys.json");
        std::fs::write(&path, b"{}").expect("file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("public");
        assert_eq!(
            KeyStore::load(path.clone())
                .err()
                .expect("reject public")
                .code,
            ErrorCode::PermissionDenied
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("private");
        std::fs::write(&path, br#"{"schema_version":2,"devices":{}}"#).expect("new schema");
        assert_eq!(
            KeyStore::load(path.clone())
                .err()
                .expect("reject newer")
                .code,
            ErrorCode::VersionMismatch
        );
        let link = root.path().join("link");
        std::os::unix::fs::symlink(path, &link).expect("symlink");
        assert!(KeyStore::load(link).is_err());
    }

    #[test]
    fn invalid_addresses_and_failed_persistence_do_not_modify_memory() {
        for address in ["A:B:C:D:E:F", "AA:BB:CC:DD:EE:FF:00", "not an address"] {
            assert!(BleKeys::new(address, [0; 16], [0; 16]).is_err());
        }
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("blocked/keys.json");
        let mut store = KeyStore::load(path).expect("not yet created");
        std::fs::write(root.path().join("blocked"), b"not a directory").expect("block parent");
        let keys = BleKeys::new("AA:BB:CC:DD:EE:FF", [0; 16], [0; 16]).expect("keys");
        assert!(store.save(&keys).is_err());
        assert!(store
            .get(keys.address())
            .expect("unchanged memory")
            .is_none());
    }
    #[test]
    fn nonregular_key_store_never_blocks_open_and_parser_diagnostics_are_redacted() {
        let root = tempfile::tempdir().expect("private directory");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private parent");
        let fifo = root.path().join("keys.fifo");
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("fifo name");
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(
            KeyStore::load(fifo).err().expect("reject FIFO").code,
            ErrorCode::PermissionDenied
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let path = root.path().join("keys.json");
        crate::storage::atomic_write(
            &path,
            br#"{"schema_version":1,"devices":{},"DO_NOT_EXPOSE_SECRET":"KEY-MATERIAL"}"#,
        )
        .expect("invalid encoded file");
        let error = KeyStore::load(path).err().expect("reject unknown fields");
        assert!(error.message.contains("line"));
        assert!(!error.message.contains("KEY-MATERIAL"));
        assert!(!error.message.contains("DO_NOT_EXPOSE_SECRET"));
    }

    #[test]
    fn oversized_and_noncanonical_key_stores_are_rejected_without_payloads() {
        let root = tempfile::tempdir().expect("private directory");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private parent");
        let path = root.path().join("keys.json");
        crate::storage::atomic_write(&path, &vec![b'x'; MAX_FILE_BYTES + 1]).expect("large file");
        assert_eq!(
            KeyStore::load(path.clone()).err().expect("oversize").code,
            ErrorCode::InvalidInput
        );
        let file = KeyFile {
            schema_version: 1,
            devices: BTreeMap::from([(
                "aa:bb:cc:dd:ee:ff".into(),
                StoredKeys {
                    irk: [1; 16],
                    encryption_key: [2; 16],
                },
            )]),
        };
        crate::storage::atomic_write(&path, &serde_json::to_vec(&file).expect("encode fixture"))
            .expect("bad identity");
        assert_eq!(
            KeyStore::load(path).err().expect("noncanonical").code,
            ErrorCode::InvalidInput
        );
    }
}
