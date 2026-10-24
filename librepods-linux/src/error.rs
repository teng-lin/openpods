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
        TransportError::Io { source, .. } => source_kind(source.as_ref()),
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

fn source_kind(source: &(dyn std::error::Error + 'static)) -> Option<ErrorKind> {
    let mut current = Some(source);
    for _ in 0..8 {
        let error = current?;
        if let Some(error) = error.downcast_ref::<PlatformError>() {
            return Some(error.kind);
        }
        if let Some(error) = error.downcast_ref::<io::Error>() {
            return Some(PlatformError::io("I/O", io::Error::from(error.kind())).kind);
        }
        #[cfg(feature = "ble")]
        {
            if let Some(error) = error.downcast_ref::<btleplug::Error>() {
                match error {
                    btleplug::Error::PermissionDenied => return Some(ErrorKind::PermissionDenied),
                    btleplug::Error::DeviceNotFound
                    | btleplug::Error::NotConnected
                    | btleplug::Error::NoAdapterAvailable => return Some(ErrorKind::Unavailable),
                    btleplug::Error::NotSupported(_) => return Some(ErrorKind::Unsupported),
                    btleplug::Error::TimedOut(_) => return Some(ErrorKind::Timeout),
                    btleplug::Error::Other(inner) => {
                        current = Some(inner.as_ref());
                        continue;
                    }
                    _ => return Some(ErrorKind::Io),
                }
            }
            let dbus_error = error.downcast_ref::<dbus::Error>().or_else(|| {
                match error.downcast_ref::<bluez_async::BluetoothError>() {
                    Some(bluez_async::BluetoothError::DbusError(error)) => Some(error),
                    _ => None,
                }
            });
            if let Some(error) = dbus_error {
                return Some(match error.name() {
                    Some(
                        "org.freedesktop.DBus.Error.AccessDenied"
                        | "org.bluez.Error.NotAuthorized"
                        | "org.bluez.Error.AuthenticationFailed",
                    ) => ErrorKind::PermissionDenied,
                    Some(
                        "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout",
                    ) => ErrorKind::Timeout,
                    Some("org.bluez.Error.NotSupported") => ErrorKind::Unsupported,
                    _ => ErrorKind::Unavailable,
                });
            }
        }
        current = error.source();
    }
    None
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
    #[cfg(feature = "ble")]
    #[test]
    fn nested_bluez_permission_and_timeout_names_are_classified_without_matching_prose() {
        for (name, expected) in [
            ("org.bluez.Error.NotAuthorized", ErrorKind::PermissionDenied),
            (
                "org.freedesktop.DBus.Error.AccessDenied",
                ErrorKind::PermissionDenied,
            ),
            ("org.freedesktop.DBus.Error.NoReply", ErrorKind::Timeout),
            ("org.bluez.Error.NotSupported", ErrorKind::Unsupported),
        ] {
            let cause = dbus::Error::new_custom(name, "localized arbitrary failure");
            let wrapped =
                TransportError::with_source("start scan", btleplug::Error::Other(Box::new(cause)));
            assert_eq!(kind(&wrapped), Some(expected));
        }
    }
}
