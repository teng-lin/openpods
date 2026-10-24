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

pub type AudioFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<serde_json::Value, DaemonError>> + Send>,
>;

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
    fn cancel(&self) {}
    fn desktop_enabled(&self) -> bool {
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
    fn audio_async(&self, address: String, operation: Operation) -> AudioFuture {
        let result = self.audio(&address, &operation);
        Box::pin(async move { result })
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
    monitor: Option<librepods_linux::bluez_monitor::BluezMonitor>,
    connecting: std::sync::Mutex<Option<librepods_linux::l2cap::Cancellation>>,
    cancelled: std::sync::atomic::AtomicBool,
}
impl Default for LinuxBackend {
    fn default() -> Self {
        Self {
            sleep: std::sync::Mutex::new(librepods_linux::sleep::SleepMonitor::start().ok()),
            monitor: librepods_linux::bluez_monitor::BluezMonitor::start().ok(),
            connecting: std::sync::Mutex::new(None),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Backend for LinuxBackend {
    fn desktop_enabled(&self) -> bool {
        true
    }
    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(connecting) = self.connecting.lock() {
            if let Some(cancel) = connecting.as_ref() {
                cancel.cancel();
            }
        }
    }
    fn sleeping(&self) -> bool {
        self.sleep
            .lock()
            .ok()
            .and_then(|monitor| monitor.as_ref().map(|monitor| monitor.is_sleeping()))
            .unwrap_or(false)
    }
    fn discover(&self, address: Option<&str>) -> Result<Endpoint, DaemonError> {
        let snapshot = self
            .monitor
            .as_ref()
            .ok_or_else(|| {
                DaemonError::transient(
                    ErrorCode::AdapterUnavailable,
                    "discovery",
                    "BlueZ monitor unavailable",
                )
            })?
            .current_snapshot();
        select_endpoint(&snapshot, address)
    }

    fn connect(
        &self,
        endpoint: &Endpoint,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        let cancel = librepods_linux::l2cap::Cancellation::new()
            .map_err(|e| transport_error("connect cancellation", e))?;
        if let Ok(mut connecting) = self.connecting.lock() {
            *connecting = Some(cancel.clone());
        }
        if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            cancel.cancel();
        }
        let result = librepods_linux::l2cap::L2capLinux::connect_with_cancel(
            &endpoint.address,
            librepods_core::protocol::packets::AAP_PSM,
            timeout,
            cancel,
        )
        .map(|transport| Arc::new(transport) as Arc<dyn SessionTransport>)
        .map_err(|e| transport_error("connect", e));
        if let Ok(mut connecting) = self.connecting.lock() {
            *connecting = None;
        }
        result
    }
    fn audio_async(&self, address: String, operation: Operation) -> AudioFuture {
        use librepods_linux::desktop::{self, AudioAction};
        Box::pin(async move {
            let action = match &operation {
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
                    let snapshot = desktop::audio_snapshot(&address)
                        .await
                        .map_err(|e| transport_error("audio status", e))?;
                    let name = snapshot
                        .outputs
                        .iter()
                        .find(|node| node.selected_device && !node.monitor)
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
            let mutation = action.is_some();
            if let Some(action) = action {
                desktop::audio_action(&address, action)
                    .await
                    .map_err(|error| {
                        let mut error = transport_error("audio action", error);
                        if matches!(
                            error.code,
                            ErrorCode::CommandTimeout
                                | ErrorCode::MalformedFrame
                                | ErrorCode::ConnectFailed
                        ) {
                            error.code = ErrorCode::OutcomeUnknown;
                            error.retryable = false;
                        }
                        error
                    })?;
            }
            let snapshot = desktop::audio_snapshot(&address).await.map_err(|error| {
                let mut error = transport_error("audio status",error);
                if mutation {
                    error.code = ErrorCode::OutcomeUnknown;
                    error.retryable = false;
                    error.message = format!("audio action returned but readback failed: {}; inspect status before repeating",error.message);
                }
                error
            })?;
            serde_json::to_value(snapshot)
                .map_err(|e| DaemonError::new(ErrorCode::Internal, "audio", e.to_string()))
        })
    }
}

fn select_endpoint(
    snapshot: &librepods_linux::bluez_monitor::BluezSnapshot,
    address: Option<&str>,
) -> Result<Endpoint, DaemonError> {
    let unavailable = || {
        DaemonError::transient(
            ErrorCode::AdapterUnavailable,
            "discovery",
            "selected Bluetooth adapter unavailable or powered off",
        )
    };
    if !snapshot.adapter_ready() {
        return Err(unavailable());
    }
    let candidates: Vec<_> = snapshot
        .devices
        .iter()
        .filter(|device| {
            device.airpods
                && device.connected
                && address.is_none_or(|address| address.eq_ignore_ascii_case(&device.address))
        })
        .collect();
    let mut ready = candidates.iter().filter(|device| {
        snapshot
            .adapters
            .iter()
            .any(|adapter| adapter.powered && adapter.path == device.adapter)
    });
    let Some(device) = ready.next() else {
        return Err(if candidates.is_empty() {
            DaemonError::transient(
                ErrorCode::DeviceUnavailable,
                "discovery",
                "selected AirPods are not connected",
            )
        } else {
            unavailable()
        });
    };
    if ready.next().is_some() {
        return Err(DaemonError::new(
            ErrorCode::InvalidInput,
            "discovery",
            "multiple AirPods connected; select an address",
        ));
    }
    Ok(Endpoint {
        address: device.address.clone(),
        name: device.name.clone(),
    })
}

pub fn transport_error(stage: &str, error: TransportError) -> DaemonError {
    use librepods_linux::error::{kind, ErrorKind};
    let code = match kind(&error) {
        Some(ErrorKind::Timeout) => ErrorCode::CommandTimeout,
        Some(ErrorKind::Cancelled) => ErrorCode::Cancelled,
        Some(ErrorKind::PermissionDenied) => ErrorCode::PermissionDenied,
        Some(ErrorKind::Unsupported) => ErrorCode::Unsupported,
        Some(ErrorKind::Unavailable) => ErrorCode::ServiceUnavailable,
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
    if matches!(
        code,
        ErrorCode::ConnectFailed | ErrorCode::ServiceUnavailable | ErrorCode::CommandTimeout
    ) {
        DaemonError::transient(code, stage, error.to_string())
    } else {
        DaemonError::new(code, stage, error.to_string())
    }
}

pub fn is_timeout(error: &TransportError) -> bool {
    matches!(error, TransportError::Timeout)
        || librepods_linux::error::kind(error) == Some(librepods_linux::error::ErrorKind::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librepods_linux::bluez_monitor::{AdapterSnapshot, BluezSnapshot, DeviceSnapshot};

    fn snapshot() -> BluezSnapshot {
        BluezSnapshot {
            available: true,
            adapters: vec![
                AdapterSnapshot {
                    path: "/org/bluez/hci0".into(),
                    address: "00:11:22:33:44:55".into(),
                    powered: true,
                },
                AdapterSnapshot {
                    path: "/org/bluez/hci1".into(),
                    address: "00:11:22:33:44:66".into(),
                    powered: true,
                },
            ],
            devices: vec![DeviceSnapshot {
                path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
                adapter: "/org/bluez/hci0".into(),
                address: "AA:BB:CC:DD:EE:FF".into(),
                name: "Selected AirPods".into(),
                paired: true,
                connected: true,
                services_resolved: true,
                airpods: true,
            }],
        }
    }

    #[test]
    fn unrelated_powered_adapter_does_not_make_selected_adapter_ready() {
        let mut state = snapshot();
        state.adapters[0].powered = false;
        for address in [None, Some("aa:bb:cc:dd:ee:ff")] {
            let error = select_endpoint(&state, address).expect_err("selected adapter off");
            assert_eq!(error.code, ErrorCode::AdapterUnavailable);
            assert!(error.retryable);
        }
        state.adapters.remove(0);
        assert_eq!(
            select_endpoint(&state, None)
                .expect_err("adapter removed")
                .code,
            ErrorCode::AdapterUnavailable
        );
        state.devices[0].adapter.clear();
        assert_eq!(
            select_endpoint(&state, None)
                .expect_err("adapter identity unknown")
                .code,
            ErrorCode::AdapterUnavailable
        );
    }

    #[test]
    fn bluez_loss_and_absent_devices_preserve_classified_discovery_failures() {
        let mut state = snapshot();
        state.available = false;
        assert_eq!(
            select_endpoint(&state, None)
                .expect_err("BlueZ lost despite cached flags")
                .code,
            ErrorCode::AdapterUnavailable
        );
        state.available = true;
        state.devices[0].connected = false;
        assert_eq!(
            select_endpoint(&state, None)
                .expect_err("device disconnected")
                .code,
            ErrorCode::DeviceUnavailable
        );
        state.devices[0].connected = true;
        state.devices[0].airpods = false;
        assert_eq!(
            select_endpoint(&state, None)
                .expect_err("unrelated Bluetooth device")
                .code,
            ErrorCode::DeviceUnavailable
        );
    }

    #[test]
    fn ambiguity_requires_selection_among_devices_on_ready_adapters() {
        let mut state = snapshot();
        let mut second = state.devices[0].clone();
        second.address = "11:22:33:44:55:66".into();
        second.name = "Other AirPods".into();
        second.adapter = "/org/bluez/hci1".into();
        state.devices.push(second);
        let error = select_endpoint(&state, None).expect_err("ambiguous connected devices");
        assert_eq!(error.code, ErrorCode::InvalidInput);
        assert!(!error.retryable);
        let endpoint = select_endpoint(&state, Some("aa:bb:cc:dd:ee:ff"))
            .expect("explicit case insensitive selection");
        assert_eq!(endpoint.address, "AA:BB:CC:DD:EE:FF");
        assert_eq!(endpoint.name, "Selected AirPods");
        assert_eq!(
            select_endpoint(&state, Some("00:00:00:00:00:00"))
                .expect_err("selected device absent")
                .code,
            ErrorCode::DeviceUnavailable
        );
        state.adapters[0].powered = false;
        assert_eq!(
            select_endpoint(&state, None)
                .expect("only one ready connected device")
                .address,
            "11:22:33:44:55:66"
        );
        assert_eq!(
            select_endpoint(&state, Some("AA:BB:CC:DD:EE:FF"))
                .expect_err("explicit selection never falls back to another device")
                .code,
            ErrorCode::AdapterUnavailable
        );
    }
}
