// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::config::EarPolicy;
use crate::snapshot::Snapshot;
use crate::{DaemonError, ErrorCode};
use librepods_core::protocol::settings::SettingChange;
use serde::{Deserialize, Serialize};

pub const MAX_FRAME: usize = 16 * 1024;
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub id: u64,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    pub operation: Operation,
}
fn default_timeout() -> u64 {
    5000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Operation {
    Status,
    Watch,
    Capabilities,
    Change(SettingChange),
    EarPolicy(EarPolicy),
    /// Opt-in scan policy; scanner failures never prevent AACP control.
    BleEnabled(bool),
    /// Explicitly request and privately persist this device's proximity keys.
    FetchKeys,
    ForgetKeys,
    Reconnect,
    Doctor,
    AudioStatus,
    SetOutput(String),
    SetInput(String),
    SetProfile(String),
    MoveStream {
        stream: String,
        output: String,
    },
    Unmute,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub id: u64,
    pub result: Option<serde_json::Value>,
    pub error: Option<DaemonError>,
}
impl Response {
    pub fn ok(id: u64, result: impl Serialize) -> Self {
        match serde_json::to_value(result) {
            Ok(value) => Self {
                version: PROTOCOL_VERSION,
                id,
                result: Some(value),
                error: None,
            },
            Err(_) => Self::failed(
                id,
                DaemonError::new(ErrorCode::Internal, "ipc", "cannot encode response"),
            ),
        }
    }
    pub fn failed(id: u64, error: DaemonError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
    pub fn snapshot(id: u64, status: &Snapshot) -> Self {
        Self::ok(id, status)
    }
}

pub fn decode_request(bytes: &[u8]) -> Result<Request, DaemonError> {
    if bytes.len() > MAX_FRAME {
        return Err(DaemonError::new(
            ErrorCode::InvalidInput,
            "ipc",
            "request exceeds 16 KiB",
        ));
    }
    let request: Request = serde_json::from_slice(bytes)
        .map_err(|e| DaemonError::new(ErrorCode::InvalidInput, "ipc", e.to_string()))?;
    if request.version != PROTOCOL_VERSION {
        return Err(DaemonError::new(
            ErrorCode::VersionMismatch,
            "ipc",
            "unsupported IPC version",
        ));
    }
    if !(100..=60_000).contains(&request.timeout_ms) {
        return Err(DaemonError::new(
            ErrorCode::InvalidInput,
            "ipc",
            "timeout must be between 100 and 60000 ms",
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_are_typed_versioned_and_bounded() {
        let request = Request {
            version: 1,
            id: 42,
            timeout_ms: 1000,
            operation: Operation::Status,
        };
        assert_eq!(
            decode_request(&serde_json::to_vec(&request).expect("json"))
                .expect("request")
                .id,
            42
        );
        assert!(decode_request(&vec![b' '; MAX_FRAME + 1]).is_err());
        assert!(decode_request(br#"{"version":2,"id":1,"operation":{"type":"status"}}"#).is_err());
        assert!(
            decode_request(br#"{"version":1,"id":1,"operation":{"type":"raw_packet"}}"#).is_err()
        );
    }
}
