// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use crate::bluez_monitor::AdapterSnapshot;
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex,
};
use zbus::zvariant::OwnedObjectPath;

const ADDRESS: &str = "AA:BB:CC:DD:EE:FF";
const ADAPTER: &str = "/org/bluez/hci0";
const PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

#[derive(Default)]
struct Shared {
    connected: AtomicBool,
    mode: AtomicU8,
    calls: Mutex<Vec<&'static str>>,
}
struct Adapter(Arc<Shared>);
#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    fn powered(&self) -> bool {
        true
    }
    async fn remove_device(
        &self,
        device: OwnedObjectPath,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<()> {
        assert_eq!(device.as_str(), PATH);
        self.0.calls.lock().unwrap().push("RemoveDevice");
        server
            .remove::<Device, _>(device)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }
}
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum BluezFailure {
    NotAuthorized(String),
    Failed(String),
    AlreadyConnected(String),
    NotConnected(String),
}
struct Device {
    shared: Arc<Shared>,
    address: &'static str,
}
#[zbus::interface(name = "org.bluez.Device1")]
impl Device {
    #[zbus(property)]
    fn address(&self) -> &str {
        self.address
    }
    #[zbus(property)]
    fn paired(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::SeqCst)
    }
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        vec![librepods_core::protocol::packets::AIRPODS_UUID.into()]
    }
    async fn connect(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Result<(), BluezFailure> {
        self.action(true, connection).await
    }
    async fn disconnect(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Result<(), BluezFailure> {
        self.action(false, connection).await
    }
}
impl Device {
    async fn action(
        &self,
        connected: bool,
        connection: &zbus::Connection,
    ) -> Result<(), BluezFailure> {
        self.shared
            .calls
            .lock()
            .unwrap()
            .push(if connected { "Connect" } else { "Disconnect" });
        match self.shared.mode.load(Ordering::SeqCst) {
            1 => return Err(BluezFailure::NotAuthorized("denied by test policy".into())),
            2 => tokio::time::sleep(Duration::from_millis(1500)).await,
            3 => return Ok(()),
            4 => return Err(BluezFailure::Failed("controller failed".into())),
            5 => {
                connection.release_name("org.bluez").await.unwrap();
            }
            6 => {
                self.shared.connected.store(connected, Ordering::SeqCst);
                return Err(if connected {
                    BluezFailure::AlreadyConnected("raced external connection".into())
                } else {
                    BluezFailure::NotConnected("raced external disconnection".into())
                });
            }
            _ => {}
        }
        self.shared.connected.store(connected, Ordering::SeqCst);
        Ok(())
    }
}
async fn service(bus: &crate::test_bus::TestBus, shared: Arc<Shared>) -> zbus::Connection {
    zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .serve_at("/", zbus::fdo::ObjectManager)
        .unwrap()
        .serve_at(ADAPTER, Adapter(shared.clone()))
        .unwrap()
        .serve_at(
            PATH,
            Device {
                shared,
                address: ADDRESS,
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap()
}
async fn client(bus: &crate::test_bus::TestBus) -> zbus::Connection {
    zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn private_bluez_confirms_connect_disconnect_and_adapter_owned_forget() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let mut prepared = 0;
    for (action, connected, paired) in [
        (DeviceAction::Connect, true, true),
        (DeviceAction::Disconnect, false, true),
        (DeviceAction::Forget, false, false),
    ] {
        let result = execute_on(
            &client,
            ADDRESS,
            action,
            Instant::now() + Duration::from_secs(2),
            &|| false,
            &mut || prepared += 1,
        )
        .await
        .unwrap();
        assert_eq!(
            (result.bluetooth_connected, result.paired, result.changed),
            (connected, paired, true)
        );
        assert_eq!(result.address, ADDRESS);
    }
    assert_eq!(prepared, 3);
    assert_eq!(
        *shared.calls.lock().unwrap(),
        ["Connect", "Disconnect", "RemoveDevice"]
    );
}

#[tokio::test]
async fn already_connected_and_disconnected_do_not_repeat_bluez_calls() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    for (action, connected) in [
        (DeviceAction::Disconnect, false),
        (DeviceAction::Connect, true),
    ] {
        shared.connected.store(connected, Ordering::SeqCst);
        let result = execute_on(
            &client,
            "",
            action,
            Instant::now() + Duration::from_secs(2),
            &|| false,
            &mut || {},
        )
        .await
        .unwrap();
        assert!(!result.changed);
        assert_eq!(result.bluetooth_connected, connected);
    }
    assert!(shared.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_and_expiry_before_dispatch_leave_bluez_and_owner_untouched() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let mut prepared = false;
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_secs(1),
        &|| true,
        &mut || prepared = true,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::Cancelled);
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now(),
        &|| false,
        &mut || prepared = true,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::TimedOut);
    assert!(!prepared);
    assert!(shared.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn post_dispatch_deadline_is_unknown_and_never_replays_even_if_bluez_completes_late() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    shared.mode.store(2, Ordering::SeqCst);
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_secs(1),
        &|| false,
        &mut || {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::OutcomeUnknown);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(shared.connected.load(Ordering::SeqCst));
    assert_eq!(*shared.calls.lock().unwrap(), ["Connect"]);
}

#[tokio::test]
async fn successful_method_without_postcondition_cannot_claim_confirmation() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    shared.mode.store(3, Ordering::SeqCst);
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_millis(100),
        &|| false,
        &mut || {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::OutcomeUnknown);
    assert_eq!(shared.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn policy_denial_retains_the_typed_error_and_dispatches_only_once() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    shared.mode.store(1, Ordering::SeqCst);
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_secs(1),
        &|| false,
        &mut || {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::PermissionDenied);
    assert!(error.message.contains("NotAuthorized"));
    assert_eq!(shared.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn losing_the_bluez_owner_after_dispatch_is_an_unknown_outcome() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    shared.mode.store(5, Ordering::SeqCst);
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_secs(1),
        &|| false,
        &mut || {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::OutcomeUnknown);
    assert_eq!(shared.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_after_owner_preparation_never_issues_a_bluez_method() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let cancelled = AtomicBool::new(false);
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_secs(1),
        &|| cancelled.load(Ordering::SeqCst),
        &mut || {
            cancelled.store(true, Ordering::SeqCst);
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::OutcomeUnknown);
    assert!(shared.calls.lock().unwrap().is_empty());
}

#[test]
fn selection_rejects_unpaired_unrelated_ambiguous_and_wrong_adapter_targets() {
    let device = DeviceSnapshot {
        path: PATH.into(),
        adapter: ADAPTER.into(),
        address: ADDRESS.into(),
        paired: true,
        airpods: true,
        ..Default::default()
    };
    let mut snapshot = BluezSnapshot {
        available: true,
        adapters: vec![AdapterSnapshot {
            path: ADAPTER.into(),
            powered: true,
            ..Default::default()
        }],
        devices: vec![device],
    };
    assert_eq!(
        select(&snapshot, "not-an-address", DeviceAction::Connect)
            .unwrap_err()
            .kind,
        FailureKind::InvalidInput
    );
    assert_eq!(
        select(&snapshot, "00:00:00:00:00:00", DeviceAction::Forget)
            .unwrap_err()
            .kind,
        FailureKind::Unavailable
    );
    snapshot.devices[0].paired = false;
    assert_eq!(
        select(&snapshot, ADDRESS, DeviceAction::Connect)
            .unwrap_err()
            .kind,
        FailureKind::Unavailable
    );
    snapshot.devices[0].paired = true;
    snapshot.devices[0].airpods = false;
    assert!(select(&snapshot, ADDRESS, DeviceAction::Forget).is_err());
    snapshot.devices[0].airpods = true;
    snapshot.adapters[0].powered = false;
    assert!(select(&snapshot, ADDRESS, DeviceAction::Connect).is_err());
    assert!(select(&snapshot, ADDRESS, DeviceAction::Forget).is_ok());
    snapshot.adapters.clear();
    assert!(select(&snapshot, ADDRESS, DeviceAction::Disconnect).is_err());
    snapshot.adapters.push(AdapterSnapshot {
        path: ADAPTER.into(),
        powered: true,
        ..Default::default()
    });
    snapshot.devices.push(snapshot.devices[0].clone());
    assert_eq!(
        select(&snapshot, "", DeviceAction::Connect)
            .unwrap_err()
            .kind,
        FailureKind::InvalidInput
    );
}

#[tokio::test]
async fn raced_bluez_idempotence_errors_still_require_fresh_postconditions() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    shared.mode.store(6, Ordering::SeqCst);
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    for action in [DeviceAction::Connect, DeviceAction::Disconnect] {
        let result = execute_on(
            &client,
            ADDRESS,
            action,
            Instant::now() + Duration::from_secs(1),
            &|| false,
            &mut || {},
        )
        .await
        .unwrap();
        assert_eq!(result.bluetooth_connected, action == DeviceAction::Connect);
    }
    shared.mode.store(4, Ordering::SeqCst);
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_secs(1),
        &|| false,
        &mut || {},
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::Failed);
    assert_eq!(
        *shared.calls.lock().unwrap(),
        ["Connect", "Disconnect", "Connect"]
    );
}
type ManagedObjects = std::collections::HashMap<
    OwnedObjectPath,
    std::collections::HashMap<
        String,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    >,
>;
struct DeniedManager(u8);
#[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
impl DeniedManager {
    fn get_managed_objects(&self) -> zbus::fdo::Result<ManagedObjects> {
        Err(match self.0 {
            0 => zbus::fdo::Error::AccessDenied("test policy denies enumeration".into()),
            1 => zbus::fdo::Error::Timeout("test query timeout".into()),
            2 => zbus::fdo::Error::InvalidArgs("test invalid query".into()),
            3 => zbus::fdo::Error::NotSupported("test unsupported query".into()),
            _ => zbus::fdo::Error::Failed("test unavailable query".into()),
        })
    }
}
#[tokio::test]
async fn enumeration_permission_denial_is_typed_before_owner_preparation() {
    let bus = crate::test_bus::TestBus::start();
    let _service = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .serve_at("/", DeniedManager(0))
        .unwrap()
        .build()
        .await
        .unwrap();
    let client = client(&bus).await;
    let mut prepared = false;
    let error = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Connect,
        Instant::now() + Duration::from_secs(1),
        &|| false,
        &mut || prepared = true,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, FailureKind::PermissionDenied);
    assert!(!prepared);
}

#[tokio::test]
async fn cancellation_while_bluez_is_running_is_unknown_without_a_compensating_operation() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    shared.mode.store(2, Ordering::SeqCst);
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let cancelled = AtomicBool::new(false);
    let check = || cancelled.load(Ordering::SeqCst);
    let mut prepare = || {};
    let (result, ()) = tokio::join!(
        execute_on(
            &client,
            ADDRESS,
            DeviceAction::Connect,
            Instant::now() + Duration::from_secs(1),
            &check,
            &mut prepare
        ),
        async {
            while shared.calls.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            cancelled.store(true, Ordering::SeqCst);
        }
    );
    assert_eq!(result.unwrap_err().kind, FailureKind::OutcomeUnknown);
    assert_eq!(*shared.calls.lock().unwrap(), ["Connect"]);
}
#[test]
fn system_bus_entrypoint_child() {
    if std::env::var_os("LPRS_DEVICE_PRIVATE_CHILD").is_none() {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let result = execute(
            ADDRESS,
            DeviceAction::Connect,
            std::time::Instant::now() + Duration::from_secs(2),
            &|| false,
            &mut || {},
        )
        .await
        .unwrap();
        assert!(result.bluetooth_connected);
    });
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_system_bus_entrypoint_runs_only_against_an_isolated_child_bus() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    let _service = service(&bus, shared.clone()).await;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "device::tests::system_bus_entrypoint_child",
                "--nocapture",
            ])
            .env("LPRS_DEVICE_PRIVATE_CHILD", "1")
            .env("DBUS_SYSTEM_BUS_ADDRESS", &bus.address)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(shared.connected.load(Ordering::SeqCst));
    assert_eq!(*shared.calls.lock().unwrap(), ["Connect"]);
}

#[tokio::test]
async fn forgetting_connected_airpods_disconnects_before_removing_the_pairing() {
    let bus = crate::test_bus::TestBus::start();
    let shared = Arc::new(Shared::default());
    shared.connected.store(true, Ordering::SeqCst);
    let _service = service(&bus, shared.clone()).await;
    let client = client(&bus).await;
    let result = execute_on(
        &client,
        ADDRESS,
        DeviceAction::Forget,
        Instant::now() + Duration::from_secs(2),
        &|| false,
        &mut || {},
    )
    .await
    .unwrap();
    assert!(!result.paired);
    assert!(!result.bluetooth_connected);
    assert_eq!(
        *shared.calls.lock().unwrap(),
        ["Disconnect", "RemoveDevice"]
    );
}
#[tokio::test]
async fn cancelled_or_denied_disconnect_does_not_continue_to_pairing_removal() {
    for denied in [false, true] {
        let bus = crate::test_bus::TestBus::start();
        let shared = Arc::new(Shared::default());
        shared.connected.store(true, Ordering::SeqCst);
        if denied {
            shared.mode.store(1, Ordering::SeqCst);
        }
        let _service = service(&bus, shared.clone()).await;
        let client = client(&bus).await;
        let error = execute_on(
            &client,
            ADDRESS,
            DeviceAction::Forget,
            Instant::now() + Duration::from_secs(1),
            &|| !shared.connected.load(Ordering::SeqCst),
            &mut || {},
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.kind,
            if denied {
                FailureKind::PermissionDenied
            } else {
                FailureKind::OutcomeUnknown
            }
        );
        assert_eq!(*shared.calls.lock().unwrap(), ["Disconnect"]);
        assert_eq!(
            snapshot_from(&client, "org.bluez")
                .await
                .unwrap()
                .devices
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn snapshot_failures_keep_their_machine_codes_before_any_owner_effect() {
    for (mode, expected) in [
        (1, FailureKind::TimedOut),
        (2, FailureKind::InvalidInput),
        (3, FailureKind::Unsupported),
        (4, FailureKind::Unavailable),
    ] {
        let bus = crate::test_bus::TestBus::start();
        let _service = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.bluez")
            .unwrap()
            .serve_at("/", DeniedManager(mode))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = client(&bus).await;
        let mut prepared = false;
        let error = execute_on(
            &client,
            ADDRESS,
            DeviceAction::Connect,
            Instant::now() + Duration::from_secs(1),
            &|| false,
            &mut || prepared = true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, expected);
        assert!(!prepared);
    }
}
