// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! BlueZ snapshots and reconciled device events with bounded D-Bus operations.

use std::collections::{BTreeMap, HashMap};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::Duration;

use futures::StreamExt;
use librepods_core::protocol::packets;
use librepods_core::transport::{DeviceEvent, DeviceMonitor, TransportError};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::error::{ErrorKind, PlatformError};

const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const EVENT_CAPACITY: usize = 32;
type Properties = HashMap<String, OwnedValue>;
type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, Properties>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterSnapshot {
    pub path: String,
    pub address: String,
    pub powered: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub path: String,
    pub adapter: String,
    pub address: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub services_resolved: bool,
    pub airpods: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluezSnapshot {
    pub available: bool,
    pub adapters: Vec<AdapterSnapshot>,
    pub devices: Vec<DeviceSnapshot>,
}

impl BluezSnapshot {
    pub fn adapter_ready(&self) -> bool {
        self.available && self.adapters.iter().any(|adapter| adapter.powered)
    }
}

fn string(properties: &Properties, key: &str) -> String {
    properties
        .get(key)
        .and_then(|value| <String>::try_from(value.clone()).ok())
        .unwrap_or_default()
}

fn boolean(properties: &Properties, key: &str) -> bool {
    properties
        .get(key)
        .and_then(|value| <bool>::try_from(value.clone()).ok())
        .unwrap_or(false)
}

fn decode(objects: ManagedObjects) -> BluezSnapshot {
    let mut snapshot = BluezSnapshot {
        available: true,
        ..BluezSnapshot::default()
    };
    for (path, interfaces) in objects {
        if let Some(properties) = interfaces.get("org.bluez.Adapter1") {
            snapshot.adapters.push(AdapterSnapshot {
                path: path.to_string(),
                address: string(properties, "Address"),
                powered: boolean(properties, "Powered"),
            });
        }
        if let Some(properties) = interfaces.get("org.bluez.Device1") {
            let uuids: Vec<String> = properties
                .get("UUIDs")
                .and_then(|value| <Vec<String>>::try_from(value.clone()).ok())
                .unwrap_or_default();
            let adapter = properties
                .get("Adapter")
                .and_then(|value| <OwnedObjectPath>::try_from(value.clone()).ok())
                .map(|value| value.to_string())
                .unwrap_or_else(|| {
                    path.rsplit_once("/dev_")
                        .map(|(prefix, _)| prefix.to_owned())
                        .unwrap_or_default()
                });
            let alias = string(properties, "Alias");
            snapshot.devices.push(DeviceSnapshot {
                path: path.to_string(),
                adapter,
                address: string(properties, "Address"),
                name: if alias.is_empty() {
                    string(properties, "Name")
                } else {
                    alias
                },
                paired: boolean(properties, "Paired"),
                connected: boolean(properties, "Connected"),
                services_resolved: boolean(properties, "ServicesResolved"),
                airpods: uuids
                    .iter()
                    .any(|uuid| uuid.eq_ignore_ascii_case(packets::AIRPODS_UUID)),
            });
        }
    }
    snapshot.adapters.sort_by(|a, b| a.path.cmp(&b.path));
    snapshot.devices.sort_by(|a, b| a.address.cmp(&b.address));
    snapshot
}

pub async fn snapshot_on(connection: &zbus::Connection) -> Result<BluezSnapshot, TransportError> {
    snapshot_from(connection, "org.bluez").await
}

pub(crate) async fn snapshot_from(
    connection: &zbus::Connection,
    destination: &str,
) -> Result<BluezSnapshot, TransportError> {
    let reply = tokio::time::timeout(
        CALL_TIMEOUT,
        connection.call_method(
            Some(destination),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        ),
    )
    .await
    .map_err(|_| PlatformError::new("query BlueZ objects", ErrorKind::Timeout))?
    .map_err(|error| TransportError::with_source("query BlueZ objects", error))?;
    let objects: ManagedObjects = reply
        .body()
        .deserialize()
        .map_err(|error| TransportError::with_source("decode BlueZ objects", error))?;
    Ok(decode(objects))
}

pub async fn snapshot() -> Result<BluezSnapshot, TransportError> {
    let connection = tokio::time::timeout(CALL_TIMEOUT, zbus::Connection::system())
        .await
        .map_err(|_| PlatformError::new("open system bus", ErrorKind::Timeout))?
        .map_err(|error| TransportError::with_source("open system bus", error))?;
    snapshot_on(&connection).await
}

pub struct BluezMonitor {
    rx: Mutex<mpsc::Receiver<DeviceEvent>>,
    latest: Arc<RwLock<BluezSnapshot>>,
    cancel: watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl BluezMonitor {
    pub fn start() -> Result<Self, TransportError> {
        Self::start_with_bus(None)
    }

    /// An explicit address allows isolated fake-BlueZ integration tests.
    pub fn start_at_bus(address: String) -> Result<Self, TransportError> {
        Self::start_with_bus(Some(address))
    }

    fn start_with_bus(address: Option<String>) -> Result<Self, TransportError> {
        let (tx, rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let (cancel, stop) = watch::channel(false);
        let latest = Arc::new(RwLock::new(BluezSnapshot::default()));
        let shared = latest.clone();
        let worker = std::thread::Builder::new()
            .name("librepods-bluez".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        log::error!("BlueZ runtime unavailable: {error}");
                        return;
                    }
                };
                runtime.block_on(run(address, shared, tx, stop));
            })
            .map_err(|error| PlatformError::io("start BlueZ monitor", error))?;
        Ok(Self {
            rx: Mutex::new(rx),
            latest,
            cancel,
            worker: Some(worker),
        })
    }

    pub fn current_snapshot(&self) -> BluezSnapshot {
        self.latest
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn next_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Option<DeviceEvent>, TransportError> {
        let receiver = self
            .rx
            .lock()
            .map_err(|_| TransportError::new("BlueZ event receiver lock poisoned"))?;
        match receiver.recv_timeout(timeout) {
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

impl Drop for BluezMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl DeviceMonitor for BluezMonitor {
    fn next_event(&self) -> Result<DeviceEvent, TransportError> {
        self.rx
            .lock()
            .map_err(|_| TransportError::new("BlueZ event receiver lock poisoned"))?
            .recv()
            .map_err(|_| TransportError::ChannelClosed)
    }
}

fn connected(snapshot: &BluezSnapshot) -> BTreeMap<String, String> {
    snapshot
        .devices
        .iter()
        .filter(|device| device.connected && device.airpods)
        .map(|device| (device.address.clone(), device.name.clone()))
        .collect()
}

fn publish(
    snapshot: BluezSnapshot,
    latest: &RwLock<BluezSnapshot>,
    sender: &mpsc::SyncSender<DeviceEvent>,
) {
    let previous = {
        let mut state = latest.write().unwrap_or_else(|error| error.into_inner());
        std::mem::replace(&mut *state, snapshot.clone())
    };
    let before = connected(&previous);
    let after = connected(&snapshot);
    for address in before
        .keys()
        .filter(|address| !after.contains_key(*address))
    {
        // The authoritative snapshot remains current even if a slow legacy consumer drops an event.
        let _ = sender.try_send(DeviceEvent::Disconnected {
            address: address.clone(),
        });
    }
    for (address, name) in after
        .iter()
        .filter(|(address, _)| !before.contains_key(*address))
    {
        let _ = sender.try_send(DeviceEvent::Connected {
            address: address.clone(),
            name: name.clone(),
        });
    }
}

async fn run(
    address: Option<String>,
    latest: Arc<RwLock<BluezSnapshot>>,
    sender: mpsc::SyncSender<DeviceEvent>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            return;
        }
        let connect = async {
            match &address {
                Some(address) => {
                    zbus::connection::Builder::address(address.as_str())?
                        .max_queued(128)
                        .build()
                        .await
                }
                None => {
                    zbus::connection::Builder::system()?
                        .max_queued(128)
                        .build()
                        .await
                }
            }
        };
        let connection = tokio::select! {
            _ = stop.changed() => return,
            result = tokio::time::timeout(CALL_TIMEOUT, connect) => match result { Ok(Ok(connection)) => connection, _ => {
                publish(BluezSnapshot::default(), &latest, &sender);
                tokio::select! { _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(1)) => continue }
            }}
        };
        let mut session_stop = stop.clone();
        let session = monitor_session(&connection, &latest, &sender, &mut session_stop);
        tokio::select! { _ = stop.changed() => return, _ = session => {} }
        publish(BluezSnapshot::default(), &latest, &sender);
        tokio::select! { _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
    }
}

async fn monitor_session(
    connection: &zbus::Connection,
    latest: &RwLock<BluezSnapshot>,
    sender: &mpsc::SyncSender<DeviceEvent>,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), TransportError> {
    // Subscribe first, then snapshot. Queued signals trigger fresh reconciliation rather than stale patches.
    let mut bluez = zbus::MessageStream::for_match_rule(
        "type='signal',sender='org.bluez'",
        connection,
        Some(128),
    )
    .await
    .map_err(|error| TransportError::with_source("subscribe BlueZ", error))?;
    let mut owner = zbus::MessageStream::for_match_rule("type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.bluez'", connection, Some(8)).await.map_err(|error| TransportError::with_source("subscribe BlueZ ownership", error))?;
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    loop {
        let current = tokio::select! { _ = stop.changed() => return Ok(()), result = snapshot_on(connection) => result };
        publish(current.unwrap_or_default(), latest, sender);
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            message = bluez.next() => if !matches!(message, Some(Ok(_))) { return Err(TransportError::ChannelClosed); },
            message = owner.next() => if !matches!(message, Some(Ok(_))) { return Err(TransportError::ChannelClosed); },
            _ = interval.tick() => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter;
    #[zbus::interface(name = "org.bluez.Adapter1")]
    impl FakeAdapter {
        #[zbus(property)]
        fn address(&self) -> &str {
            "00:11:22:33:44:55"
        }
        #[zbus(property)]
        fn powered(&self) -> bool {
            true
        }
    }

    struct FakeDevice {
        uuids: Vec<String>,
    }
    #[zbus::interface(name = "org.bluez.Device1")]
    impl FakeDevice {
        #[zbus(property)]
        fn address(&self) -> &str {
            "AA:BB:CC:DD:EE:FF"
        }
        #[zbus(property)]
        fn alias(&self) -> &str {
            "Renamed buds"
        }
        #[zbus(property)]
        fn paired(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn connected(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn services_resolved(&self) -> bool {
            !self.uuids.is_empty()
        }
        #[zbus(property, name = "UUIDs")]
        fn uuids(&self) -> Vec<String> {
            self.uuids.clone()
        }
    }

    async fn wait_for(monitor: &BluezMonitor, predicate: impl Fn(&BluezSnapshot) -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if predicate(&monitor.current_snapshot()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("monitor reconciles private bus state");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn private_bus_tracks_late_uuids_and_bluez_owner_restart() {
        let bus = crate::test_bus::TestBus::start();
        let service = zbus::connection::Builder::address(bus.address.as_str())
            .expect("bus builder")
            .name("org.bluez")
            .expect("name")
            .serve_at("/", zbus::fdo::ObjectManager)
            .expect("object manager")
            .serve_at("/org/bluez/hci0", FakeAdapter)
            .expect("adapter")
            .serve_at(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
                FakeDevice { uuids: vec![] },
            )
            .expect("device")
            .build()
            .await
            .expect("fake BlueZ");
        let mut monitor = BluezMonitor::start_at_bus(bus.address.clone()).expect("monitor");
        wait_for(&monitor, |snapshot| {
            snapshot.adapter_ready() && snapshot.devices.len() == 1
        })
        .await;
        assert!(!monitor.current_snapshot().devices[0].airpods);
        let device = service
            .object_server()
            .interface::<_, FakeDevice>("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF")
            .await
            .expect("device interface");
        device.get_mut().await.uuids = vec![packets::AIRPODS_UUID.to_uppercase()];
        device
            .get()
            .await
            .u_u_i_ds_changed(device.signal_emitter())
            .await
            .expect("UUID signal");
        wait_for(&monitor, |snapshot| {
            snapshot
                .devices
                .first()
                .is_some_and(|device| device.airpods)
        })
        .await;
        assert!(matches!(
            monitor.next_event_timeout(Duration::from_secs(1)),
            Ok(Some(DeviceEvent::Connected { .. }))
        ));
        service
            .release_name("org.bluez")
            .await
            .expect("release BlueZ");
        wait_for(&monitor, |snapshot| !snapshot.available).await;
        assert!(matches!(
            monitor.next_event_timeout(Duration::from_secs(1)),
            Ok(Some(DeviceEvent::Disconnected { .. }))
        ));
        service
            .request_name("org.bluez")
            .await
            .expect("restart BlueZ");
        wait_for(&monitor, |snapshot| {
            snapshot.available
                && snapshot
                    .devices
                    .first()
                    .is_some_and(|device| device.airpods)
        })
        .await;
        assert!(matches!(
            monitor.next_event_timeout(Duration::from_secs(1)),
            Ok(Some(DeviceEvent::Connected { .. }))
        ));
        assert!(
            matches!(
                monitor.next_event_timeout(Duration::from_millis(20)),
                Ok(None)
            ),
            "reconciled snapshots must not duplicate connection events"
        );
        monitor.shutdown();
    }

    fn pods(connected: bool) -> BluezSnapshot {
        BluezSnapshot {
            available: true,
            adapters: vec![AdapterSnapshot {
                path: "/org/bluez/hci0".into(),
                powered: true,
                ..Default::default()
            }],
            devices: vec![DeviceSnapshot {
                address: "AA:BB:CC:DD:EE:FF".into(),
                name: "Renamed buds".into(),
                connected,
                airpods: true,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn snapshots_deduplicate_and_service_loss_disconnects() {
        let (tx, rx) = mpsc::sync_channel(8);
        let state = RwLock::new(BluezSnapshot::default());
        publish(pods(true), &state, &tx);
        assert!(matches!(rx.try_recv(), Ok(DeviceEvent::Connected { .. })));
        publish(pods(true), &state, &tx);
        assert!(rx.try_recv().is_err());
        publish(BluezSnapshot::default(), &state, &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(DeviceEvent::Disconnected { .. })
        ));
        assert!(!state.read().expect("state").adapter_ready());
    }

    #[test]
    fn late_service_identification_is_reported_once_and_slow_consumers_do_not_block() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let state = RwLock::new(BluezSnapshot::default());
        let mut unknown = pods(true);
        unknown.devices[0].airpods = false;
        publish(unknown, &state, &tx);
        publish(pods(true), &state, &tx);
        publish(pods(false), &state, &tx);
        assert!(!state.read().expect("state").devices[0].connected);
    }
}
