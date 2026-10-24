// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Structured failures at Linux platform boundaries.

use std::io;

use librepods_core::transport::TransportError;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidInput,
    PermissionDenied,
    Unavailable,
    Timeout,
    Cancelled,
    Truncated,
    Unsupported,
    Io,
}

#[derive(Debug, Error)]
#[error("{operation}: {kind:?}{detail}")]
pub struct PlatformError {
    pub operation: &'static str,
    pub kind: ErrorKind,
    detail: String,
    #[source]
    pub source: Option<io::Error>,
}

impl PlatformError {
    pub fn new(operation: &'static str, kind: ErrorKind) -> Self {
        Self {
            operation,
            kind,
            detail: String::new(),
            source: None,
        }
    }

    pub fn io(operation: &'static str, error: io::Error) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            io::ErrorKind::TimedOut => ErrorKind::Timeout,
            io::ErrorKind::Unsupported => ErrorKind::Unsupported,
            io::ErrorKind::NotConnected | io::ErrorKind::NotFound => ErrorKind::Unavailable,
            _ => ErrorKind::Io,
        };
        Self {
            operation,
            kind,
            detail: format!(": {error}"),
            source: Some(error),
        }
    }
}

impl From<PlatformError> for TransportError {
    fn from(error: PlatformError) -> Self {
        TransportError::with_source(error.to_string(), error)
    }
}

pub fn kind(error: &TransportError) -> Option<ErrorKind> {
    match error {
        TransportError::Io { source, .. } => source
            .downcast_ref::<PlatformError>()
            .map(|error| error.kind),
        TransportError::Timeout => Some(ErrorKind::Timeout),
        TransportError::Cancelled => Some(ErrorKind::Cancelled),
        TransportError::Unsupported => Some(ErrorKind::Unsupported),
        TransportError::Truncated { .. } => Some(ErrorKind::Truncated),
        TransportError::PermissionDenied => Some(ErrorKind::PermissionDenied),
        TransportError::NotConnected
        | TransportError::ConnectionClosed
        | TransportError::ChannelClosed => Some(ErrorKind::Unavailable),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_core_and_platform_error_classification() {
        for (error, expected) in [
            (TransportError::Timeout, ErrorKind::Timeout),
            (TransportError::Cancelled, ErrorKind::Cancelled),
            (TransportError::Unsupported, ErrorKind::Unsupported),
            (
                TransportError::Truncated { limit: 10 },
                ErrorKind::Truncated,
            ),
            (
                TransportError::PermissionDenied,
                ErrorKind::PermissionDenied,
            ),
            (TransportError::ChannelClosed, ErrorKind::Unavailable),
            (
                PlatformError::new("test", ErrorKind::InvalidInput).into(),
                ErrorKind::InvalidInput,
            ),
            (
                PlatformError::io("test", io::Error::from(io::ErrorKind::TimedOut)).into(),
                ErrorKind::Timeout,
            ),
        ] {
            assert_eq!(kind(&error), Some(expected));
        }
        assert_eq!(kind(&TransportError::new("unknown")), None);
    }
}
