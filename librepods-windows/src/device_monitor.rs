// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Windows device monitor using the WinRT Bluetooth APIs.
//!
//! Windows has no BlueZ/D-Bus equivalent. Instead we periodically enumerate
//! paired Bluetooth devices through `Windows.Devices.Bluetooth` and emit
//! connect/disconnect events when the set of connected AirPods changes.
//!
//! AirPods are identified by their friendly name containing "AirPods" — this is
//! a heuristic, since the AAP service UUID is not exposed by the WinRT API.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use librepods_core::transport::{DeviceEvent, DeviceMonitor, TransportError};
use tokio::sync::mpsc;
use windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothDevice};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Poll interval between device enumeration passes.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Windows Bluetooth device monitor.
pub struct WindowsDeviceMonitor {
    rx: Mutex<mpsc::UnboundedReceiver<DeviceEvent>>,
    /// Keeps the polling thread alive.
    _handle: std::thread::JoinHandle<()>,
}

impl WindowsDeviceMonitor {
    /// Create a new monitor and start watching for AirPods connection events.
    pub fn start() -> Result<Self, TransportError> {
        let (tx, rx) = mpsc::unbounded_channel();

        let handle = std::thread::spawn(move || {
            // WinRT async operations require the thread to be COM-initialised.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            poll_loop(tx);
        });

        Ok(Self {
            rx: Mutex::new(rx),
            _handle: handle,
        })
    }
}

impl DeviceMonitor for WindowsDeviceMonitor {
    fn next_event(&self) -> Result<DeviceEvent, TransportError> {
        let mut rx = self
            .rx
            .lock()
            .map_err(|_| TransportError::new("lock poisoned"))?;
        loop {
            match rx.try_recv() {
                Ok(event) => return Ok(event),
                Err(mpsc::error::TryRecvError::Empty) => {
                    drop(rx);
                    std::thread::sleep(Duration::from_millis(100));
                    rx = self
                        .rx
                        .lock()
                        .map_err(|_| TransportError::new("lock poisoned"))?;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(TransportError::ChannelClosed);
                }
            }
        }
    }
}

/// Poll connected AirPods and diff against the previous snapshot.
fn poll_loop(tx: mpsc::UnboundedSender<DeviceEvent>) {
    let mut connected: HashMap<String, String> = HashMap::new();

    loop {
        match enumerate_airpods() {
            Ok(found) => {
                let now: HashMap<String, String> = found.into_iter().collect();

                for (address, name) in &now {
                    if !connected.contains_key(address) {
                        log::info!("AirPods connected: {name} ({address})");
                        let _ = tx.send(DeviceEvent::Connected {
                            address: address.clone(),
                            name: name.clone(),
                        });
                    }
                }
                for address in connected.keys() {
                    if !now.contains_key(address) {
                        log::info!("AirPods disconnected: {address}");
                        let _ = tx.send(DeviceEvent::Disconnected {
                            address: address.clone(),
                        });
                    }
                }

                connected = now;
            }
            Err(e) => log::debug!("AirPods enumeration failed: {e}"),
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Enumerate paired, currently-connected AirPods devices.
fn enumerate_airpods() -> Result<Vec<(String, String)>, TransportError> {
    let selector = BluetoothDevice::GetDeviceSelectorFromPairingState(true)
        .map_err(|e| TransportError::with_source("BluetoothDevice selector failed", e))?;

    let infos = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .map_err(|e| TransportError::with_source("DeviceInformation enumeration failed", e))?
        .join()
        .map_err(|e| TransportError::with_source("device enumeration failed", e))?;

    let count = infos
        .Size()
        .map_err(|e| TransportError::with_source("device list size failed", e))?;

    let mut result = Vec::new();
    for i in 0..count {
        let info = match infos.GetAt(i) {
            Ok(info) => info,
            Err(_) => continue,
        };
        let id = match info.Id() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let device = match BluetoothDevice::FromIdAsync(&id).and_then(|op| op.join()) {
            Ok(device) => device,
            Err(_) => continue,
        };

        let is_connected = matches!(
            device.ConnectionStatus(),
            Ok(BluetoothConnectionStatus::Connected)
        );
        if !is_connected {
            continue;
        }

        let name = device.Name().map(|n| n.to_string()).unwrap_or_default();
        if !name.to_ascii_lowercase().contains("airpods") {
            continue;
        }

        let address = device
            .BluetoothAddress()
            .map(format_mac)
            .unwrap_or_default();

        result.push((address, name));
    }

    Ok(result)
}

/// Format a `BTH_ADDR` (48-bit big-endian) as `AA:BB:CC:DD:EE:FF`.
fn format_mac(addr: u64) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        (addr >> 40) & 0xFF,
        (addr >> 32) & 0xFF,
        (addr >> 24) & 0xFF,
        (addr >> 16) & 0xFF,
        (addr >> 8) & 0xFF,
        addr & 0xFF,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mac_is_big_endian() {
        assert_eq!(format_mac(0xAABBCCDDEEFF), "AA:BB:CC:DD:EE:FF");
    }
}
