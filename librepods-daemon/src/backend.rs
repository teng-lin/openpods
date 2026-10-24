// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{ipc::Operation, DaemonError, ErrorCode};
use librepods_core::transport::{L2capTransport, TransportError};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub address: String,
    pub name: String,
}

pub trait SessionTransport: Send + Sync {
    fn send(&self, bytes: &[u8], timeout: Duration) -> Result<(), TransportError>;
    fn recv(&self, bytes: &mut [u8], timeout: Duration) -> Result<usize, TransportError>;
    fn close(&self);
}

pub trait Backend: Send + Sync {
    fn discover(&self, address: Option<&str>) -> Result<Endpoint, DaemonError>;
    fn connect(
        &self,
        endpoint: &Endpoint,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionTransport>, DaemonError>;
    fn sleeping(&self) -> bool {
        false
    }
    fn audio(
        &self,
        _address: &str,
        _operation: &Operation,
    ) -> Result<serde_json::Value, DaemonError> {
        Err(DaemonError::new(
            ErrorCode::Unsupported,
            "audio",
            "desktop adapter unavailable",
        ))
    }
}

impl SessionTransport for librepods_linux::l2cap::L2capLinux {
    fn send(&self, bytes: &[u8], timeout: Duration) -> Result<(), TransportError> {
        self.send_timeout(bytes, timeout)
    }
    fn recv(&self, bytes: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        self.recv_timeout(bytes, timeout)
    }
    fn close(&self) {
        let _ = L2capTransport::close(self);
    }
}

pub struct LinuxBackend {
    sleep: std::sync::Mutex<Option<librepods_linux::sleep::SleepMonitor>>,
    sleeping: std::sync::atomic::AtomicBool,
}
impl Default for LinuxBackend {
    fn default() -> Self {
        Self {
            sleep: std::sync::Mutex::new(librepods_linux::sleep::SleepMonitor::start().ok()),
            sleeping: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Backend for LinuxBackend {
    fn sleeping(&self) -> bool {
        if let Ok(monitor) = self.sleep.lock() {
            if let Some(monitor) = monitor.as_ref() {
                while let Ok(Some(sleeping)) = monitor.next_event_timeout(Duration::ZERO) {
                    self.sleeping
                        .store(sleeping, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        self.sleeping.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn discover(&self, address: Option<&str>) -> Result<Endpoint, DaemonError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DaemonError::io("discovery runtime", e))?;
        let snapshot = rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(3),
                    librepods_linux::bluez_monitor::snapshot(),
                )
                .await
            })
            .map_err(|_| {
                DaemonError::transient(
                    ErrorCode::AdapterUnavailable,
                    "discovery",
                    "BlueZ query timed out",
                )
            })?
            .map_err(|e| {
                DaemonError::transient(ErrorCode::AdapterUnavailable, "discovery", e.to_string())
            })?;
        if !snapshot.adapter_ready() {
            return Err(DaemonError::transient(
                ErrorCode::AdapterUnavailable,
                "discovery",
                "Bluetooth adapter unavailable or powered off",
            ));
        }
        let mut devices = snapshot.devices.into_iter().filter(|device| {
            device.airpods
                && device.connected
                && address.is_none_or(|a| a.eq_ignore_ascii_case(&device.address))
        });
        let device = devices.next().ok_or_else(|| {
            DaemonError::transient(
                ErrorCode::DeviceUnavailable,
                "discovery",
                "selected AirPods are not connected",
            )
        })?;
        if devices.next().is_some() {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "discovery",
                "multiple AirPods connected; select an address",
            ));
        }
        Ok(Endpoint {
            address: device.address,
            name: device.name,
        })
    }
    fn connect(
        &self,
        endpoint: &Endpoint,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        librepods_linux::l2cap::L2capLinux::connect_timeout(&endpoint.address, timeout)
            .map(|transport| Arc::new(transport) as Arc<dyn SessionTransport>)
            .map_err(|e| transport_error("connect", e))
    }
    fn audio(
        &self,
        address: &str,
        operation: &Operation,
    ) -> Result<serde_json::Value, DaemonError> {
        use librepods_linux::desktop::{self, AudioAction};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DaemonError::io("audio runtime", e))?;
        rt.block_on(async {
            let action = match operation {
                Operation::AudioStatus => None,
                Operation::SetOutput(name) => {
                    Some(AudioAction::SetDefaultOutput { name: name.clone() })
                }
                Operation::SetInput(name) => {
                    Some(AudioAction::SetDefaultInput { name: name.clone() })
                }
                Operation::SetProfile(profile) => Some(AudioAction::SetProfile {
                    profile: profile.clone(),
                }),
                Operation::MoveStream { stream, output } => Some(AudioAction::MovePlayback {
                    index: stream.parse().map_err(|_| {
                        DaemonError::new(ErrorCode::InvalidInput, "audio", "stream must be numeric")
                    })?,
                    output: output.clone(),
                }),
                Operation::Unmute => {
                    let snapshot = desktop::audio_snapshot(address)
                        .await
                        .map_err(|e| transport_error("audio status", e))?;
                    let name = snapshot
                        .outputs
                        .first()
                        .ok_or_else(|| {
                            DaemonError::new(
                                ErrorCode::DeviceUnavailable,
                                "audio",
                                "selected AirPods output unavailable",
                            )
                        })?
                        .name
                        .clone();
                    Some(AudioAction::SetMute {
                        name,
                        input: false,
                        muted: false,
                    })
                }
                _ => {
                    return Err(DaemonError::new(
                        ErrorCode::Unsupported,
                        "audio",
                        "unsupported audio operation",
                    ))
                }
            };
            if let Some(action) = action {
                desktop::audio_action(address, action)
                    .await
                    .map_err(|e| transport_error("audio action", e))?;
            }
            let snapshot = desktop::audio_snapshot(address)
                .await
                .map_err(|e| transport_error("audio status", e))?;
            serde_json::to_value(snapshot)
                .map_err(|e| DaemonError::new(ErrorCode::Internal, "audio", e.to_string()))
        })
    }
}

pub fn transport_error(stage: &str, error: TransportError) -> DaemonError {
    use librepods_linux::error::{kind, ErrorKind};
    let code = match kind(&error) {
        Some(ErrorKind::Timeout) => ErrorCode::CommandTimeout,
        Some(ErrorKind::Cancelled) => ErrorCode::Cancelled,
        Some(ErrorKind::PermissionDenied) => ErrorCode::PermissionDenied,
        Some(ErrorKind::Unsupported) => ErrorCode::Unsupported,
        Some(ErrorKind::Truncated) => ErrorCode::MalformedFrame,
        Some(ErrorKind::InvalidInput) => ErrorCode::InvalidInput,
        _ => match &error {
            TransportError::Timeout => ErrorCode::CommandTimeout,
            TransportError::Cancelled => ErrorCode::Cancelled,
            TransportError::PermissionDenied => ErrorCode::PermissionDenied,
            TransportError::Unsupported => ErrorCode::Unsupported,
            _ => ErrorCode::ConnectFailed,
        },
    };
    DaemonError::new(code, stage, error.to_string())
}

pub fn is_timeout(error: &TransportError) -> bool {
    matches!(error, TransportError::Timeout)
        || librepods_linux::error::kind(error) == Some(librepods_linux::error::ErrorKind::Timeout)
}
