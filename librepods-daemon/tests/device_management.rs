// SPDX-License-Identifier: AGPL-3.0-only
#![cfg(target_os = "linux")]
use librepods_core::{
    device::state::NoiseControlMode,
    protocol::{packets, settings::SettingChange},
    transport::{DeviceAction, DeviceActionResult, TransportError},
};
use librepods_daemon::{
    backend::{Backend, Endpoint, SessionTransport},
    config::Config,
    ipc::{Operation, Request},
    session::Session,
    snapshot::Snapshot,
    DaemonError, ErrorCode,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
const ADDRESS: &str = "00:11:22:33:44:55";
#[derive(Default)]
struct Facts {
    connected: AtomicBool,
    sleeping: AtomicBool,
    epoch: AtomicU64,
    calls: Mutex<Vec<DeviceAction>>,
    connections: AtomicUsize,
    writes: AtomicUsize,
    deny: AtomicBool,
    deny_after_prepare: AtomicBool,
    wrong_reply_identity: AtomicBool,
    stall: AtomicBool,
    preflight_stall: AtomicBool,
    preflight_seen: AtomicBool,
    suspend_after_success: AtomicBool,
    audio_started: AtomicBool,
    audio_effects: AtomicUsize,
    audio_release: tokio::sync::Notify,
}
struct Fake(Arc<Facts>);
impl Backend for Fake {
    fn discover(&self, _: Option<&str>) -> Result<Endpoint, DaemonError> {
        if !self.0.connected.load(Ordering::SeqCst) {
            return Err(DaemonError::transient(
                ErrorCode::DeviceUnavailable,
                "discovery",
                "not connected",
            ));
        }
        Ok(Endpoint {
            address: ADDRESS.into(),
            name: "Synthetic Pods".into(),
        })
    }
    fn connect(&self, _: &Endpoint, _: Duration) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        self.0.connections.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Peer {
            facts: self.0.clone(),
            queue: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            wake: Condvar::new(),
        }))
    }
    fn audio_async(&self, _: String, _: Operation) -> librepods_daemon::backend::AudioFuture {
        let facts = self.0.clone();
        Box::pin(async move {
            facts.audio_started.store(true, Ordering::SeqCst);
            facts.audio_release.notified().await;
            facts.audio_effects.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"profile":"synthetic"}))
        })
    }
    fn sleeping(&self) -> bool {
        self.0.sleeping.load(Ordering::SeqCst)
    }
    fn suspend_epoch(&self) -> u64 {
        self.0.epoch.load(Ordering::SeqCst)
    }
    fn device_action(
        &self,
        address: &str,
        action: DeviceAction,
        deadline: Instant,
        cancelled: &(dyn Fn() -> bool + Sync),
        prepare: &mut dyn FnMut(),
    ) -> Result<DeviceActionResult, DaemonError> {
        assert!(address.is_empty() || address.eq_ignore_ascii_case(ADDRESS));
        self.0.preflight_seen.store(true, Ordering::SeqCst);
        while self.0.preflight_stall.load(Ordering::SeqCst)
            && !cancelled()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        if cancelled() {
            return Err(DaemonError::new(
                ErrorCode::Cancelled,
                "preflight",
                "cancelled before dispatch",
            ));
        }
        if Instant::now() >= deadline {
            return Err(DaemonError::new(
                ErrorCode::CommandTimeout,
                "preflight",
                "deadline",
            ));
        }
        if self.0.deny.load(Ordering::SeqCst) {
            return Err(DaemonError::new(
                ErrorCode::PermissionDenied,
                "preflight",
                "denied",
            ));
        }
        prepare();
        self.0.calls.lock().unwrap().push(action);
        if self.0.deny_after_prepare.load(Ordering::SeqCst) {
            return Err(DaemonError::new(
                ErrorCode::PermissionDenied,
                "BlueZ method",
                "policy denied operation",
            ));
        }
        while self.0.stall.load(Ordering::SeqCst) && !cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if cancelled() || Instant::now() >= deadline {
            return Err(DaemonError::new(
                ErrorCode::OutcomeUnknown,
                "device",
                "reply lost after dispatch",
            ));
        }
        let connected = action == DeviceAction::Connect;
        self.0.connected.store(connected, Ordering::SeqCst);
        if self.0.suspend_after_success.load(Ordering::SeqCst) {
            self.0.epoch.fetch_add(1, Ordering::SeqCst);
        }
        Ok(DeviceActionResult {
            address: if self.0.wrong_reply_identity.load(Ordering::SeqCst) {
                "00:11:22:33:44:66"
            } else {
                ADDRESS
            }
            .into(),
            bluetooth_connected: connected,
            paired: action != DeviceAction::Forget,
            changed: true,
        })
    }
}
struct Peer {
    facts: Arc<Facts>,
    queue: Mutex<VecDeque<Vec<u8>>>,
    closed: AtomicBool,
    wake: Condvar,
}
impl SessionTransport for Peer {
    fn send(&self, data: &[u8], _: Duration) -> Result<(), TransportError> {
        let reply = if data == packets::HANDSHAKE {
            Some(packets::HANDSHAKE_ACK_PREFIX.to_vec())
        } else if data == packets::SET_SPECIFIC_FEATURES {
            Some(packets::FEATURES_ACK_PREFIX.to_vec())
        } else if data == packets::REQUEST_NOTIFICATIONS {
            let mut metadata = vec![4, 0, 4, 0, 0x1d, 0, 2, 0xd5, 0, 4, 0];
            metadata.extend_from_slice(b"Synthetic Pods\0A3048\0Apple Inc.\0");
            Some(metadata)
        } else {
            self.facts.writes.fetch_add(1, Ordering::SeqCst);
            None
        };
        if let Some(reply) = reply {
            self.queue.lock().unwrap().push_back(reply);
            if data == packets::REQUEST_NOTIFICATIONS {
                self.queue.lock().unwrap().push_back(
                    packets::build_noise_control(NoiseControlMode::NoiseCancellation).to_vec(),
                );
            }
            self.wake.notify_all();
        }
        Ok(())
    }
    fn recv(&self, bytes: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        let queue = self.queue.lock().unwrap();
        let (mut queue, _) = self
            .wake
            .wait_timeout_while(queue, timeout, |q| {
                q.is_empty() && !self.closed.load(Ordering::SeqCst)
            })
            .unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(TransportError::ConnectionClosed);
        }
        let packet = queue.pop_front().ok_or(TransportError::Timeout)?;
        bytes[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }
}
fn session(facts: Arc<Facts>) -> Session {
    Session::spawn(
        Arc::new(Fake(facts)),
        Config {
            address: Some(ADDRESS.into()),
            ..Config::default()
        },
        "device-test".into(),
    )
}
async fn wait(
    status: &mut watch::Receiver<Snapshot>,
    condition: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let d = status.borrow_and_update().clone();
            if condition(&d) {
                return d;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("bounded actor transition")
}
async fn call(
    session: &Session,
    operation: Operation,
    timeout_ms: u64,
) -> librepods_daemon::ipc::Response {
    session
        .handle
        .request(Request {
            version: 1,
            id: 42,
            timeout_ms,
            operation,
        })
        .await
}
fn target(operation: Operation) -> Operation {
    Operation::ForDevice {
        address: ADDRESS.into(),
        operation: Box::new(operation),
    }
}
async fn count(facts: &Facts, n: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while facts.calls.lock().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(2)).await
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn connect_confirms_bluetooth_then_opens_aacp_and_disconnect_stays_disconnected() {
    let facts = Arc::new(Facts::default());
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    let response = call(&session, target(Operation::DeviceConnect), 1000).await;
    assert!(response.error.is_none(), "{response:?}");
    let value = response.result.unwrap();
    assert_eq!(value["device"]["bluetooth_connected"], true);
    assert_eq!(value["aacp_ready"], false);
    wait(&mut status, |d| {
        d.connected && d.device.model_number == "A3048"
    })
    .await;
    let before = facts.connections.load(Ordering::SeqCst);
    let response = call(&session, Operation::DeviceDisconnect, 1000).await;
    assert!(response.error.is_none());
    assert!(!status.borrow().connection_desired);
    assert!(!status.borrow().connected);
    // Even an external Bluetooth reconnect and sleep/resume cannot override intent.
    facts.connected.store(true, Ordering::SeqCst);
    facts.sleeping.store(true, Ordering::SeqCst);
    wait(&mut status, |d| d.lifecycle == "suspended").await;
    facts.sleeping.store(false, Ordering::SeqCst);
    wait(&mut status, |d| d.lifecycle != "suspended").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(facts.connections.load(Ordering::SeqCst), before);
    assert!(!status.borrow().connection_desired);
    assert!(call(&session, Operation::Reconnect, 1000)
        .await
        .error
        .is_none());
    wait(&mut status, |d| d.connected).await;
    assert_eq!(
        *facts.calls.lock().unwrap(),
        [DeviceAction::Connect, DeviceAction::Disconnect]
    );
    session.stop();
}

#[tokio::test]
async fn forget_requires_both_native_confirmation_and_explicit_identity() {
    let facts = Arc::new(Facts::default());
    let session = session(facts.clone());
    for op in [
        Operation::DeviceForget { confirmed: true },
        target(Operation::DeviceForget { confirmed: false }),
    ] {
        assert_eq!(
            call(&session, op, 1000).await.error.unwrap().code,
            ErrorCode::InvalidInput
        );
    }
    assert!(facts.calls.lock().unwrap().is_empty());
    let response = call(
        &session,
        target(Operation::DeviceForget { confirmed: true }),
        1000,
    )
    .await;
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(response.result.unwrap()["device"]["paired"], false);
    assert!(!session.handle.status.borrow().connection_desired);
    session.stop();
}

#[tokio::test]
async fn invalid_target_and_preflight_denial_do_not_tear_down_a_healthy_link() {
    let facts = Arc::new(Facts::default());
    facts.connected.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    wait(&mut status, |d| d.connected).await;
    let before = status.borrow().session_generation;
    let other = Operation::ForDevice {
        address: "00:11:22:33:44:66".into(),
        operation: Box::new(Operation::DeviceDisconnect),
    };
    assert_eq!(
        call(&session, other, 1000).await.error.unwrap().code,
        ErrorCode::DeviceUnavailable
    );
    facts.deny.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&session, Operation::DeviceDisconnect, 1000)
            .await
            .error
            .unwrap()
            .code,
        ErrorCode::PermissionDenied
    );
    assert!(status.borrow().connected);
    assert!(status.borrow().connection_desired);
    assert_eq!(status.borrow().session_generation, before);
    assert!(facts.calls.lock().unwrap().is_empty());
    session.stop();
}

#[tokio::test]
async fn disconnect_preempts_a_written_setting_and_never_replays_it() {
    let facts = Arc::new(Facts::default());
    facts.connected.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    wait(&mut status, |d| d.device.model_number == "A3048").await;
    let handle = session.handle.clone();
    let pending = tokio::spawn(async move {
        handle
            .request(Request {
                version: 1,
                id: 1,
                timeout_ms: 2000,
                operation: Operation::Change(SettingChange::ListeningMode(
                    NoiseControlMode::Transparency,
                )),
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while facts.writes.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await
        }
    })
    .await
    .unwrap();
    assert!(call(&session, Operation::DeviceDisconnect, 1000)
        .await
        .error
        .is_none());
    assert_eq!(
        pending.await.unwrap().error.unwrap().code,
        ErrorCode::OutcomeUnknown
    );
    assert_eq!(facts.writes.load(Ordering::SeqCst), 1);
    assert!(!status.borrow().connection_desired);
    session.stop();
}

#[tokio::test]
async fn client_cancellation_before_dispatch_preserves_the_original_connection() {
    let facts = Arc::new(Facts::default());
    facts.connected.store(true, Ordering::SeqCst);
    facts.preflight_stall.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    wait(&mut status, |d| d.connected).await;
    let handle = session.handle.clone();
    let pending = tokio::spawn(async move {
        handle
            .request(Request {
                version: 1,
                id: 1,
                timeout_ms: 2000,
                operation: Operation::DeviceDisconnect,
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !facts.preflight_seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await
        }
    })
    .await
    .unwrap();
    pending.abort();
    let _ = pending.await;
    facts.preflight_stall.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(status.borrow().connected);
    assert!(status.borrow().connection_desired);
    assert!(facts.calls.lock().unwrap().is_empty());
    session.stop();
}

#[tokio::test]
async fn dispatched_timeout_and_suspend_never_schedule_late_connects_or_replay() {
    let facts = Arc::new(Facts::default());
    facts.stall.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let response = call(&session, Operation::DeviceConnect, 100).await;
    assert_eq!(response.error.unwrap().code, ErrorCode::OutcomeUnknown);
    count(&facts, 1).await;
    facts.stall.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(facts.calls.lock().unwrap().len(), 1);
    assert_eq!(facts.connections.load(Ordering::SeqCst), 0);
    assert!(!session.handle.status.borrow().connection_desired);
    facts.suspend_after_success.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&session, Operation::DeviceConnect, 1000)
            .await
            .error
            .unwrap()
            .code,
        ErrorCode::OutcomeUnknown
    );
    assert_eq!(facts.connections.load(Ordering::SeqCst), 0);
    session.stop();
}

#[tokio::test]
async fn device_management_cancels_inflight_audio_before_a_late_effect_or_success() {
    for readonly in [false, true] {
        let facts = Arc::new(Facts::default());
        facts.connected.store(true, Ordering::SeqCst);
        let session = session(facts.clone());
        let mut status = session.handle.status.clone();
        wait(&mut status, |d| d.connected).await;
        let handle = session.handle.clone();
        let pending = tokio::spawn(async move {
            handle
                .request(Request {
                    version: 1,
                    id: 1,
                    timeout_ms: 2000,
                    operation: if readonly {
                        Operation::AudioStatus
                    } else {
                        Operation::SetProfile("synthetic".into())
                    },
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !facts.audio_started.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(2)).await
            }
        })
        .await
        .unwrap();
        assert!(call(&session, Operation::DeviceDisconnect, 1000)
            .await
            .error
            .is_none());
        assert_eq!(
            pending.await.unwrap().error.unwrap().code,
            if readonly {
                ErrorCode::Cancelled
            } else {
                ErrorCode::OutcomeUnknown
            }
        );
        facts.audio_release.notify_one();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(facts.audio_effects.load(Ordering::SeqCst), 0);
        session.stop();
    }
}
fn paths(root: &std::path::Path) -> librepods_daemon::storage::Paths {
    librepods_daemon::storage::Paths {
        runtime: root.join("runtime"),
        state: root.join("state"),
        config: root.join("config"),
        legacy_state: root.join("legacy-state"),
        legacy_socket: root.join("legacy-socket"),
    }
}
#[tokio::test]
async fn successful_forget_removes_only_selected_private_keys_and_reports_cleanup_failure() {
    use librepods_daemon::keys::{BleKeys, KeyStore};
    for fail_cleanup in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.state).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&paths.state, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let file = paths.state.join("ble-keys.json");
        let mut store = KeyStore::load(file.clone()).unwrap();
        store
            .save(&BleKeys::new(ADDRESS, [1; 16], [2; 16]).unwrap())
            .unwrap();
        store
            .save(&BleKeys::new("00:11:22:33:44:66", [3; 16], [4; 16]).unwrap())
            .unwrap();
        if fail_cleanup {
            std::fs::remove_file(&file).unwrap();
            std::fs::create_dir(&file).unwrap();
        }
        let facts = Arc::new(Facts::default());
        let session = Session::spawn_persistent(
            Arc::new(Fake(facts.clone())),
            Config {
                address: Some(ADDRESS.into()),
                ..Config::default()
            },
            "keys-forget-test".into(),
            paths,
        );
        let response = call(
            &session,
            target(Operation::DeviceForget { confirmed: true }),
            1000,
        )
        .await;
        if fail_cleanup {
            let error = response.error.unwrap();
            assert_eq!(error.code, ErrorCode::PersistenceFailed);
            assert!(error.message.contains("pairing removed"));
            assert!(!error.retryable);
        } else {
            assert!(response.error.is_none(), "{response:?}");
            let store = KeyStore::load(file).unwrap();
            assert!(store.get(ADDRESS).unwrap().is_none());
            assert!(store.get("00:11:22:33:44:66").unwrap().is_some());
        }
        assert_eq!(*facts.calls.lock().unwrap(), [DeviceAction::Forget]);
        assert!(!session.handle.status.borrow().connection_desired);
        session.stop();
    }
}

#[tokio::test]
async fn explicit_connection_selects_a_paired_device_before_any_aacp_identity_exists() {
    let facts = Arc::new(Facts::default());
    let session = Session::spawn(
        Arc::new(Fake(facts.clone())),
        Config::default(),
        "initial-device".into(),
    );
    assert!(session.handle.status.borrow().device.address.is_empty());
    let response = call(&session, target(Operation::DeviceConnect), 1000).await;
    assert!(response.error.is_none(), "{response:?}");
    let mut status = session.handle.status.clone();
    wait(&mut status, |d| {
        d.connected && d.device.model_number == "A3048"
    })
    .await;
    assert_eq!(status.borrow().device.address, ADDRESS);
    session.stop();
}

#[test]
fn older_native_status_defaults_to_normal_connection_intent() {
    let mut value = serde_json::to_value(Snapshot::new(
        "previous-version".into(),
        &Default::default(),
    ))
    .unwrap();
    value.as_object_mut().unwrap().remove("connection_desired");
    let snapshot: Snapshot = serde_json::from_value(value).unwrap();
    assert!(snapshot.connection_desired);
}

#[tokio::test]
async fn later_explicit_device_intents_keep_fifo_order_while_old_settings_are_cancelled() {
    use std::{
        future::{poll_fn, Future},
        task::Poll,
    };
    let facts = Arc::new(Facts::default());
    facts.connected.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    wait(&mut status, |d| d.connected).await;
    facts.preflight_stall.store(true, Ordering::SeqCst);
    let handle = session.handle.clone();
    let first = tokio::spawn(async move {
        handle
            .request(Request {
                version: 1,
                id: 10,
                timeout_ms: 1000,
                operation: Operation::DeviceDisconnect,
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !facts.preflight_seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await
        }
    })
    .await
    .unwrap();
    let mut setting = Box::pin(session.handle.request(Request {
        version: 1,
        id: 11,
        timeout_ms: 1000,
        operation: Operation::Change(SettingChange::ListeningMode(NoiseControlMode::Transparency)),
    }));
    let mut second = Box::pin(session.handle.request(Request {
        version: 1,
        id: 12,
        timeout_ms: 1000,
        operation: Operation::DeviceConnect,
    }));
    // Admission is synchronous before the first reply wait; both are in the channel.
    poll_fn(|cx| {
        assert!(setting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    poll_fn(|cx| {
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    facts.preflight_stall.store(false, Ordering::SeqCst);
    assert!(first.await.unwrap().error.is_none());
    assert_eq!(setting.await.error.unwrap().code, ErrorCode::Cancelled);
    let response = second.await;
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        *facts.calls.lock().unwrap(),
        [DeviceAction::Disconnect, DeviceAction::Connect]
    );
    assert_eq!(facts.writes.load(Ordering::SeqCst), 0);
    session.stop();
}

#[tokio::test]
async fn a_definitive_bluez_refusal_restores_the_prior_control_policy_without_replaying() {
    let facts = Arc::new(Facts::default());
    facts.connected.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    wait(&mut status, |d| d.connected).await;
    facts.deny_after_prepare.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&session, Operation::DeviceDisconnect, 1000)
            .await
            .error
            .unwrap()
            .code,
        ErrorCode::PermissionDenied
    );
    assert!(status.borrow().connection_desired);
    wait(&mut status, |d| d.connected).await;
    assert_eq!(*facts.calls.lock().unwrap(), [DeviceAction::Disconnect]);
    session.stop();
}
#[tokio::test]
async fn mismatched_backend_identity_cannot_redirect_control_or_key_cleanup() {
    let facts = Arc::new(Facts::default());
    facts.wrong_reply_identity.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let response = call(
        &session,
        target(Operation::DeviceForget { confirmed: true }),
        1000,
    )
    .await;
    assert_eq!(response.error.unwrap().code, ErrorCode::OutcomeUnknown);
    assert_eq!(session.handle.status.borrow().device.address, ADDRESS);
    assert!(!session.handle.status.borrow().connection_desired);
    session.stop();
}
