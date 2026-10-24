// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Optional, independently owned BLE scanner. No runtime backend in a default build.

#[cfg(not(feature = "ble"))]
mod disabled {
    use crate::error::{ErrorKind, PlatformError};
    use librepods_core::transport::{BleScanner, RawBleAdvertisement, TransportError};

    pub struct BtleplugScanner;

    impl BtleplugScanner {
        pub async fn start() -> Result<Self, TransportError> {
            Err(PlatformError::new("BLE feature is not compiled in", ErrorKind::Unsupported).into())
        }
    }

    impl BleScanner for BtleplugScanner {
        fn next_advertisement(&self) -> Result<RawBleAdvertisement, TransportError> {
            Err(PlatformError::new("BLE feature is not compiled in", ErrorKind::Unsupported).into())
        }
    }
}

#[cfg(not(feature = "ble"))]
pub use disabled::BtleplugScanner;

#[cfg(feature = "ble")]
mod enabled {
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral, ScanFilter};
    use btleplug::platform::{Adapter, Manager};
    use futures::StreamExt;
    use librepods_core::transport::{BleScanner, RawBleAdvertisement, TransportError};
    use tokio::sync::watch;

    use crate::error::{ErrorKind, PlatformError};

    const QUEUE_CAPACITY: usize = 64;
    const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
    const APPLE_COMPANY_ID: u16 = 0x004C;

    pub struct BtleplugScanner {
        rx: Mutex<mpsc::Receiver<RawBleAdvertisement>>,
        cancel: watch::Sender<bool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl BtleplugScanner {
        /// The worker owns a continuously driven runtime, independent of the caller's runtime.
        pub async fn start() -> Result<Self, TransportError> {
            let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (cancel, mut stop) = watch::channel(false);
            let worker = std::thread::Builder::new().name("librepods-ble".into()).spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(error) => { let _ = ready_tx.send(Err(PlatformError::io("create BLE runtime", error).into())); return; }
                };
                runtime.block_on(async {
                    let adapter = tokio::select! {
                        _ = stop.changed() => return,
                        result = tokio::time::timeout(OPERATION_TIMEOUT, initialize()) => match result {
                            Ok(Ok(adapter)) => adapter,
                            Ok(Err(error)) => { let _ = ready_tx.send(Err(error)); return; }
                            Err(_) => { let _ = ready_tx.send(Err(PlatformError::new("start BLE scan", ErrorKind::Timeout).into())); return; }
                        }
                    };
                    let _ = ready_tx.send(Ok(()));
                    let mut worker_stop = stop.clone();
                    tokio::select! {
                        _ = stop.changed() => {},
                        result = scan(&adapter, &tx, &mut worker_stop) => if let Err(error) = result { log::warn!("BLE scanner stopped: {error}"); }
                    }
                    let _ = tokio::time::timeout(OPERATION_TIMEOUT, adapter.stop_scan()).await;
                });
            }).map_err(|error| PlatformError::io("start BLE worker", error))?;
            let ready =
                tokio::task::spawn_blocking(move || ready_rx.recv_timeout(Duration::from_secs(3)))
                    .await
                    .map_err(|error| TransportError::with_source("wait for BLE startup", error));
            let startup = match ready {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => {
                    Err(PlatformError::new("wait for BLE startup", ErrorKind::Timeout).into())
                }
                Err(error) => Err(error),
            };
            if let Err(error) = startup {
                let _ = cancel.send(true);
                let _ = worker.join();
                return Err(error);
            }
            Ok(Self {
                rx: Mutex::new(rx),
                cancel,
                worker: Some(worker),
            })
        }

        pub fn next_advertisement_timeout(
            &self,
            timeout: Duration,
        ) -> Result<Option<RawBleAdvertisement>, TransportError> {
            match self
                .rx
                .lock()
                .map_err(|_| TransportError::new("BLE event lock poisoned"))?
                .recv_timeout(timeout)
            {
                Ok(event) => Ok(Some(event)),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(TransportError::ChannelClosed),
            }
        }

        pub fn shutdown(&mut self) {
            let _ = self.cancel.send(true);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    impl Drop for BtleplugScanner {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    impl BleScanner for BtleplugScanner {
        fn next_advertisement(&self) -> Result<RawBleAdvertisement, TransportError> {
            self.rx
                .lock()
                .map_err(|_| TransportError::new("BLE event lock poisoned"))?
                .recv()
                .map_err(|_| TransportError::ChannelClosed)
        }
    }

    async fn initialize() -> Result<Adapter, TransportError> {
        let manager = Manager::new()
            .await
            .map_err(|error| TransportError::with_source("open BLE manager", error))?;
        let adapter = manager
            .adapters()
            .await
            .map_err(|error| TransportError::with_source("enumerate BLE adapters", error))?
            .into_iter()
            .next()
            .ok_or_else(|| {
                PlatformError::new("no BLE adapter available", ErrorKind::Unavailable)
            })?;
        adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|error| TransportError::with_source("start BLE scan", error))?;
        Ok(adapter)
    }

    async fn scan(
        adapter: &Adapter,
        sender: &mpsc::SyncSender<RawBleAdvertisement>,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<(), TransportError> {
        let mut events = adapter
            .events()
            .await
            .map_err(|error| TransportError::with_source("subscribe BLE events", error))?;
        loop {
            let event = tokio::select! { _ = stop.changed() => return Ok(()), event = events.next() => event.ok_or(TransportError::ChannelClosed)? };
            let id = match event {
                CentralEvent::ManufacturerDataAdvertisement { id, .. }
                | CentralEvent::DeviceDiscovered(id)
                | CentralEvent::DeviceUpdated(id) => id,
                _ => continue,
            };
            let properties = tokio::time::timeout(OPERATION_TIMEOUT, async {
                let peripheral = adapter.peripheral(&id).await?;
                peripheral.properties().await
            })
            .await;
            let properties = match properties {
                Ok(Ok(Some(properties))) => properties,
                _ => continue,
            };
            let Some(data) = properties.manufacturer_data.get(&APPLE_COMPANY_ID) else {
                continue;
            };
            // Forward raw manufacturer data. Identity and protocol validation belong to core.
            let event = RawBleAdvertisement {
                address: properties.address.to_string(),
                manufacturer_data: data.clone(),
                rssi: properties.rssi,
            };
            match sender.try_send(event) {
                Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
    }
}

#[cfg(feature = "ble")]
pub use enabled::BtleplugScanner;

#[cfg(all(test, not(feature = "ble")))]
mod tests {
    use super::*;
    use crate::error::{kind, ErrorKind};

    #[tokio::test]
    async fn default_build_reports_ble_unavailable_without_starting_a_worker() {
        let error = match BtleplugScanner::start().await {
            Ok(_) => panic!("BLE absent"),
            Err(error) => error,
        };
        assert_eq!(kind(&error), Some(ErrorKind::Unsupported));
    }
}
