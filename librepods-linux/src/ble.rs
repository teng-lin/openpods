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
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral, ScanFilter};
    use btleplug::platform::{Adapter, Manager};
    use futures::StreamExt;
    use librepods_core::transport::{BleScanner, RawBleAdvertisement, TransportError};
    use tokio::sync::watch;

    use crate::error::{ErrorKind, PlatformError};

    const QUEUE_CAPACITY: usize = 64;
    const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(1);
    const STOP_TIMEOUT: Duration = Duration::from_millis(500);
    const APPLE_COMPANY_ID: u16 = 0x004C;

    pub struct BtleplugScanner {
        rx: Mutex<mpsc::Receiver<RawBleAdvertisement>>,
        cancel: watch::Sender<bool>,
        worker: Option<std::thread::JoinHandle<()>>,
        stop_error: Arc<Mutex<Option<Arc<TransportError>>>>,
    }

    impl BtleplugScanner {
        /// The worker owns a continuously driven runtime, independent of the caller's runtime.
        pub async fn start() -> Result<Self, TransportError> {
            let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (cancel, mut stop) = watch::channel(false);
            let stop_error = Arc::new(Mutex::new(None));
            let worker_stop_error = stop_error.clone();
            let worker = std::thread::Builder::new().name("librepods-ble".into()).spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(error) => { let _ = ready_tx.send(Err(PlatformError::io("create BLE runtime", error).into())); return; }
                };
                runtime.block_on(async {
                    let adapter = tokio::select! {
                        _ = stop.changed() => return,
                        result = tokio::time::timeout(STARTUP_TIMEOUT, initialize()) => match result {
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
                    let result = match tokio::time::timeout(STOP_TIMEOUT, adapter.stop_scan()).await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(TransportError::with_source("stop BLE scan", error)),
                        Err(_) => Err(PlatformError::new("stop BLE scan", ErrorKind::Timeout).into()),
                    };
                    if let Err(error) = result {
                        log::warn!("BLE scan cleanup degraded; releasing scan client: {error}");
                        *worker_stop_error.lock().unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(error));
                    }
                });
            }).map_err(|error| PlatformError::io("start BLE worker", error))?;
            let ready =
                tokio::task::spawn_blocking(move || ready_rx.recv_timeout(Duration::from_secs(2)))
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
                stop_error,
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

        /// The worker and its BlueZ client are released even when StopDiscovery fails.
        pub fn shutdown_checked(&mut self) -> Result<(), TransportError> {
            self.shutdown();
            let error = self
                .stop_error
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            match error {
                Some(error) => Err(TransportError::with_source(
                    "BLE cleanup failed after client release",
                    error,
                )),
                None => Ok(()),
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

#[cfg(all(test, feature = "ble"))]
mod native_tests {
    use super::*;
    use librepods_core::transport::BleScanner;
    use std::collections::HashMap;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    struct FakeAdapter {
        stops: Arc<AtomicUsize>,
        starts: Arc<AtomicUsize>,
        client: Arc<std::sync::Mutex<Option<String>>>,
        mode: String,
    }
    #[zbus::interface(name = "org.bluez.Adapter1")]
    impl FakeAdapter {
        #[zbus(property)]
        fn address(&self) -> &str {
            "00:11:22:33:44:55"
        }
        #[zbus(property)]
        fn address_type(&self) -> &str {
            "public"
        }
        #[zbus(property)]
        fn name(&self) -> &str {
            "test adapter"
        }
        #[zbus(property)]
        fn alias(&self) -> &str {
            "test adapter"
        }
        #[zbus(property)]
        fn modalias(&self) -> &str {
            "usb:v1234p5678d90AB"
        }
        #[zbus(property)]
        fn powered(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn set_powered(&mut self, _value: bool) {}
        #[zbus(property)]
        fn discovering(&self) -> bool {
            self.starts.load(Ordering::SeqCst) > self.stops.load(Ordering::SeqCst)
        }
        fn set_discovery_filter(&self, _properties: HashMap<String, zbus::zvariant::OwnedValue>) {}
        async fn start_discovery(
            &self,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<()> {
            *self.client.lock().expect("client lock") =
                header.sender().map(|sender| sender.to_string());
            if self.mode == "deny" {
                return Err(zbus::fdo::Error::AccessDenied(
                    "injected permission denial".into(),
                ));
            }
            if self.mode == "stall" {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn stop_discovery(&self) {
            if self.mode == "stop-stall" {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct FakeDevice;
    #[zbus::interface(name = "org.bluez.Device1")]
    impl FakeDevice {
        #[zbus(property)]
        fn address(&self) -> &str {
            "70:81:94:0D:FB:AA"
        }
        #[zbus(property)]
        fn address_type(&self) -> &str {
            "random"
        }
        #[zbus(property)]
        fn paired(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn connected(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn services_resolved(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn trusted(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn blocked(&self) -> bool {
            false
        }
        #[zbus(property)]
        fn legacy_pairing(&self) -> bool {
            false
        }
        #[zbus(property, name = "RSSI")]
        fn rssi(&self) -> i16 {
            -42
        }
        #[zbus(property)]
        fn manufacturer_data(&self) -> HashMap<u16, zbus::zvariant::OwnedValue> {
            HashMap::from([(
                0x004c,
                zbus::zvariant::Value::from(vec![1u8, 2, 3])
                    .try_into()
                    .expect("manufacturer variant"),
            )])
        }
    }
    #[test]
    fn native_worker_private_bluez_runtime_events_queue_and_stop_are_isolated() {
        const CHILD: &str = "LIBREPODS_BLE_PRIVATE_BUS";
        if let Some(address) = std::env::var_os(CHILD) {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("fake BlueZ runtime");
            runtime.block_on(async {
                let stops = Arc::new(AtomicUsize::new(0));
                let starts = Arc::new(AtomicUsize::new(0));
                let client = Arc::new(std::sync::Mutex::new(None));
                let mode = std::env::var("LIBREPODS_BLE_CASE").expect("isolated case");
                let builder =
                    zbus::connection::Builder::address(address.to_str().expect("bus address"))
                        .expect("bus")
                        .name("org.bluez")
                        .expect("owner")
                        .serve_at("/", zbus::fdo::ObjectManager)
                        .expect("object manager");
                let builder = if mode == "absent" {
                    builder
                } else {
                    builder
                        .serve_at(
                            "/org/bluez/hci0",
                            FakeAdapter {
                                stops: stops.clone(),
                                starts: starts.clone(),
                                client: client.clone(),
                                mode: mode.clone(),
                            },
                        )
                        .expect("adapter")
                        .serve_at("/org/bluez/hci0/dev_70_81_94_0D_FB_AA", FakeDevice)
                        .expect("device")
                };
                let service = builder.build().await.expect("fake BlueZ");
                let startup = std::time::Instant::now();
                let scanner = BtleplugScanner::start().await;
                if mode != "normal" && mode != "stop-stall" {
                    let error = scanner.err().expect("injected startup failure");
                    let expected = match mode.as_str() {
                        "deny" => crate::error::ErrorKind::PermissionDenied,
                        "absent" => crate::error::ErrorKind::Unavailable,
                        "stall" => crate::error::ErrorKind::Timeout,
                        _ => panic!("unknown test case"),
                    };
                    assert_eq!(crate::error::kind(&error), Some(expected), "{error:?}");
                    assert!(startup.elapsed() < Duration::from_secs(2));
                    let disconnected_client = client.lock().expect("client").clone();
                    if let Some(client) = disconnected_client {
                        let bus = zbus::fdo::DBusProxy::new(&service)
                            .await
                            .expect("bus proxy");
                        assert!(!bus
                            .name_has_owner(client.as_str().try_into().expect("unique name"))
                            .await
                            .expect("client released"));
                    }
                    return;
                }
                let mut scanner = scanner.expect("native scanner starts on fake BlueZ");
                assert_eq!(starts.load(Ordering::SeqCst), 1);
                let emit = || async {
                    let properties = HashMap::from([(
                        "ManufacturerData",
                        zbus::zvariant::Value::from(FakeDevice.manufacturer_data()),
                    )]);
                    service
                        .emit_signal(
                            None::<&str>,
                            "/org/bluez/hci0/dev_70_81_94_0D_FB_AA",
                            "org.freedesktop.DBus.Properties",
                            "PropertiesChanged",
                            &("org.bluez.Device1", properties, Vec::<String>::new()),
                        )
                        .await
                        .expect("advertisement signal");
                };
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                let observed = loop {
                    emit().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    if let Some(event) = scanner
                        .next_advertisement_timeout(Duration::ZERO)
                        .expect("scanner events")
                    {
                        break event;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "advertisement deadline"
                    );
                };
                assert_eq!(observed.address, "70:81:94:0D:FB:AA");
                assert_eq!(observed.manufacturer_data, vec![1, 2, 3]);
                assert_eq!(observed.rssi, Some(-42));
                emit().await;
                let observed =
                    tokio::task::block_in_place(|| BleScanner::next_advertisement(&scanner))
                        .expect("blocking trait events");
                assert_eq!(observed.manufacturer_data, vec![1, 2, 3]);
                for _ in 0..200 {
                    emit().await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let start = std::time::Instant::now();
                let stopped = scanner.shutdown_checked();
                scanner.shutdown();
                assert!(start.elapsed() < Duration::from_secs(1));
                if mode == "stop-stall" {
                    assert_eq!(
                        crate::error::kind(&stopped.expect_err("stop deadline")),
                        Some(crate::error::ErrorKind::Timeout)
                    );
                    assert!(
                        scanner.shutdown_checked().is_err(),
                        "cleanup failure remains observable"
                    );
                } else {
                    stopped.expect("confirmed stop");
                    assert_eq!(
                        stops.load(Ordering::SeqCst),
                        1,
                        "native Drop issues StopDiscovery"
                    );
                }
                // Freeze the producer before counting: draining a live queue can consume >capacity.
                let mut received = 0;
                while let Ok(Some(_)) = scanner.next_advertisement_timeout(Duration::ZERO) {
                    received += 1;
                }
                assert!(received > 0 && received <= 64, "bounded queue: {received}");
                assert!(scanner.next_advertisement_timeout(Duration::ZERO).is_err());
                let client = client
                    .lock()
                    .expect("client")
                    .clone()
                    .expect("scan client identity");
                let bus = zbus::fdo::DBusProxy::new(&service)
                    .await
                    .expect("bus proxy");
                assert!(
                    !bus.name_has_owner(client.as_str().try_into().expect("unique name"))
                        .await
                        .expect("client released"),
                    "scan client must be gone before AACP connect"
                );
                drop(service);
            });
            return;
        }
        for mode in ["normal", "deny", "absent", "stall", "stop-stall"] {
            let bus = crate::test_bus::TestBus::start();
            let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "ble::native_tests::native_worker_private_bluez_runtime_events_queue_and_stop_are_isolated", "--nocapture"])
                .env(CHILD, &bus.address).env("DBUS_SYSTEM_BUS_ADDRESS", &bus.address).env("LIBREPODS_BLE_CASE", mode)
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().expect("isolated child");
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            while child.try_wait().expect("child status").is_none() {
                if std::time::Instant::now() >= deadline {
                    child.kill().expect("stop stalled fake child");
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let result = child.wait_with_output().expect("isolated BlueZ child");
            assert!(
                result.status.success(),
                "case {mode}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}
