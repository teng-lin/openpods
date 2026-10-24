// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    Unsupported,
    PermissionDenied,
    AdapterUnavailable,
    DeviceUnavailable,
    ConnectFailed,
    HandshakeTimeout,
    MalformedFrame,
    CommandTimeout,
    OutcomeUnknown,
    Superseded,
    Cancelled,
    QueueFull,
    ServiceUnavailable,
    VersionMismatch,
    PersistenceFailed,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("{stage}: {message}")]
pub struct DaemonError {
    pub code: ErrorCode,
    pub stage: String,
    pub message: String,
    pub retryable: bool,
}

impl DaemonError {
    pub fn new(code: ErrorCode, stage: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            stage: stage.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub fn transient(
        code: ErrorCode,
        stage: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            retryable: true,
            ..Self::new(code, stage, message)
        }
    }

    pub fn io(stage: &str, error: std::io::Error) -> Self {
        let code = match error.kind() {
            std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            std::io::ErrorKind::TimedOut => ErrorCode::CommandTimeout,
            _ => ErrorCode::Internal,
        };
        Self::new(code, stage, error.to_string())
    }
}
