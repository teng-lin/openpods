// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

#![cfg(target_os = "linux")]

use librepods_core::device::state::{DeviceState, NoiseControlMode};
use librepods_core::protocol::settings::SettingChange;
use librepods_core::protocol::{control_command::ControlCommand, packets, parser};
use librepods_core::transport::TransportError;
use librepods_daemon::backend::{Backend, Endpoint, SessionTransport};
use librepods_daemon::config::{Config, EarPolicy};
use librepods_daemon::ipc::{Operation, Request, Response};
use librepods_daemon::runtime;
use librepods_daemon::snapshot::Snapshot;
use librepods_daemon::storage::Paths;
use librepods_daemon::{DaemonError, ErrorCode};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::oneshot;

const ADDRESS: &str = "00:11:22:33:44:55";

fn metadata() -> Vec<u8> {
    let mut bytes = packets::METADATA_PREFIX.to_vec();
    bytes.extend_from_slice(&[0; 6]);
    bytes.extend_from_slice(b"Example AirPods\0A3048\0Apple Inc.\0");
    bytes
}

fn battery_left() -> Vec<u8> {
    // A single left component reports a genuine 100% discharging reading.
    vec![4, 0, 4, 0, 4, 0, 1, 4, 1, 100, 2, 1]
}

#[derive(Default)]
struct Shared {
    sleeping: AtomicBool,
    absent: AtomicBool,
    lose_ack: AtomicBool,
    no_handshake: AtomicBool,
    connections: AtomicUsize,
    mode: Mutex<Option<NoiseControlMode>>,
    changes: Mutex<Vec<Vec<u8>>>,
    peers: Mutex<Vec<Arc<Peer>>>,
}

struct Peer {
    shared: Arc<Shared>,
    incoming: Mutex<VecDeque<Vec<u8>>>,
    available: Condvar,
    closed: AtomicBool,
}

impl Peer {
    fn push(&self, bytes: Vec<u8>) {
        self.incoming.lock().expect("peer queue").push_back(bytes);
        self.available.notify_one();
    }
}

impl SessionTransport for Peer {
    fn send(&self, bytes: &[u8], _: Duration) -> Result<(), TransportError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(TransportError::ConnectionClosed);
        }
        if bytes == packets::HANDSHAKE {
            if !self.shared.no_handshake.load(Ordering::SeqCst) {
                self.push(packets::HANDSHAKE_ACK_PREFIX.to_vec());
            }
        } else if bytes == packets::SET_SPECIFIC_FEATURES {
            self.push(packets::FEATURES_ACK_PREFIX.to_vec());
        } else if bytes == packets::REQUEST_NOTIFICATIONS {
            self.push(metadata());
            self.push(battery_left());
            let mode = self
                .shared
                .mode
                .lock()
                .expect("mode")
                .unwrap_or(NoiseControlMode::NoiseCancellation);
            self.push(packets::build_noise_control(mode).to_vec());
            self.push(ControlCommand::enabled(0x28).to_vec());
            self.push(ControlCommand::disabled(0x1B).to_vec());
        } else {
            self.shared
                .changes
                .lock()
                .expect("changes")
                .push(bytes.to_vec());
            if let Ok(parser::ParsedPacket::NoiseControl(mode)) = parser::parse(bytes) {
                *self.shared.mode.lock().expect("mode") = Some(mode);
            }
            if !self.shared.lose_ack.load(Ordering::SeqCst) {
                self.push(bytes.to_vec());
            }
        }
        Ok(())
    }

    fn recv(&self, bytes: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        let queue = self.incoming.lock().expect("peer queue");
        let (mut queue, _) = self
            .available
            .wait_timeout_while(queue, timeout, |queue| {
                queue.is_empty() && !self.closed.load(Ordering::SeqCst)
            })
            .expect("peer wait");
        if self.closed.load(Ordering::SeqCst) {
            return Err(TransportError::ConnectionClosed);
        }
        let packet = queue.pop_front().ok_or(TransportError::Timeout)?;
        if packet.len() > bytes.len() {
            return Err(TransportError::new("emulated truncated datagram"));
        }
        bytes[..packet.len()].copy_from_slice(&packet);
        Ok(packet.len())
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.available.notify_all();
    }
}

struct EmulatedBackend(Arc<Shared>);

impl Backend for EmulatedBackend {
    fn discover(&self, _: Option<&str>) -> Result<Endpoint, DaemonError> {
        if self.0.absent.load(Ordering::SeqCst) {
            return Err(DaemonError::transient(
                ErrorCode::AdapterUnavailable,
                "emulator",
                "adapter absent",
            ));
        }
        Ok(Endpoint {
            address: ADDRESS.into(),
            name: "Example AirPods".into(),
        })
    }

    fn connect(&self, _: &Endpoint, _: Duration) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        self.0.connections.fetch_add(1, Ordering::SeqCst);
        let peer = Arc::new(Peer {
            shared: self.0.clone(),
            incoming: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        self.0.peers.lock().expect("peers").push(peer.clone());
        Ok(peer)
    }

    fn sleeping(&self) -> bool {
        self.0.sleeping.load(Ordering::SeqCst)
    }

    fn audio(&self, _: &str, operation: &Operation) -> Result<serde_json::Value, DaemonError> {
        if matches!(operation, Operation::AudioStatus) {
            Ok(serde_json::json!({"health":"usable","profile":"a2dp-sink","capture":"external"}))
        } else {
            Err(DaemonError::new(
                ErrorCode::Unsupported,
                "emulator",
                "audio mutation disabled",
            ))
        }
    }
}

struct Harness {
    _directory: TempDir,
    paths: Paths,
    shared: Arc<Shared>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), DaemonError>>>,
}

impl Harness {
    async fn start(shared: Arc<Shared>, config: Config) -> Self {
        let directory = TempDir::new().expect("test directory");
        let root = directory.path();
        let paths = Paths {
            runtime: root.join("run/librepods-rs"),
            state: root.join("state/librepods-rs"),
            config: root.join("config/librepods-rs/config.json"),
            legacy_state: root.join("state/librepods/status.json"),
            legacy_socket: root.join("run/librepods.sock"),
        };
        let (sender, receiver) = oneshot::channel();
        let backend = Arc::new(EmulatedBackend(shared.clone()));
        let server_paths = paths.clone();
        let task = tokio::spawn(runtime::run(server_paths, config, backend, async {
            let _ = receiver.await;
        }));
        let harness = Self {
            _directory: directory,
            paths,
            shared,
            shutdown: Some(sender),
            task: Some(task),
        };
        harness.wait_for(|_| true).await;
        harness
    }

    async fn request(&self, id: u64, operation: Operation, timeout_ms: u64) -> Response {
        runtime::request(
            &self.paths,
            &Request {
                version: 1,
                id,
                timeout_ms,
                operation,
            },
        )
        .await
        .expect("native IPC response")
    }

    async fn wait_for(&self, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let request = Request {
                version: 1,
                id: 1,
                timeout_ms: 1000,
                operation: Operation::Status,
            };
            let result = runtime::request(&self.paths, &request).await;
            let diagnostic = format!("{result:?}");
            if let Ok(response) = result {
                if let Some(value) = response.result {
                    let snapshot: Snapshot = serde_json::from_value(value).expect("status schema");
                    if predicate(&snapshot) {
                        return snapshot;
                    }
                }
            }
            assert!(Instant::now() < deadline, "status condition did not become true: {diagnostic}; server finished={}", self.task.as_ref().is_some_and(|task|task.is_finished()));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn ready(&self) -> Snapshot {
        self.wait_for(|status| {
            status.connected
                && status.device.model_number == "A3048"
                && status.device.listening_mode.is_some()
        })
        .await
    }

    async fn stop(mut self) {
        let _ = self.shutdown.take().expect("shutdown sender").send(());
        tokio::time::timeout(
            Duration::from_secs(2),
            self.task.take().expect("server task"),
        )
        .await
        .expect("bounded server stop")
        .expect("server join")
        .expect("server result");
        assert!(
            !self.paths.socket().exists(),
            "owned socket survives clean stop"
        );
        assert!(
            !self.paths.state_file().exists(),
            "owned snapshot survives clean stop"
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_ipc_preserves_handshake_partial_battery_and_serialized_confirmations() {
    let harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    let status = harness.ready().await;
    assert!(!status.ble_enabled);
    assert_eq!(status.device.battery.left.level, 100);
    assert!(!librepods_daemon::snapshot::available(
        status.device.battery.case_
    ));
    let (mode, awareness) = tokio::join!(
        harness.request(
            10,
            Operation::Change(SettingChange::ListeningMode(NoiseControlMode::Transparency)),
            1000
        ),
        harness.request(
            11,
            Operation::Change(SettingChange::ConversationalAwareness(false)),
            1000
        ),
    );
    assert!(mode.error.is_none(), "{:?}", mode.error);
    assert!(awareness.error.is_none(), "{:?}", awareness.error);
    assert_eq!(mode.result, Some(serde_json::json!("Confirmed")));
    assert_eq!(awareness.result, Some(serde_json::json!("Confirmed")));
    assert_eq!(harness.shared.changes.lock().expect("changes").len(), 2);
    harness
        .wait_for(|snapshot| snapshot.device.listening_mode == Some(NoiseControlMode::Transparency))
        .await;
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_ack_is_uncertain_and_recovery_does_not_replay_the_mutation() {
    let shared = Arc::new(Shared::default());
    let harness = Harness::start(shared.clone(), Config::default()).await;
    harness.ready().await;
    shared.lose_ack.store(true, Ordering::SeqCst);
    let response = runtime::request(
        &harness.paths,
        &Request {
            version: 1,
            id: 20,
            timeout_ms: 150,
            operation: Operation::Change(SettingChange::ListeningMode(
                NoiseControlMode::Transparency,
            )),
        },
    )
    .await;
    let error = match response {
        Ok(response) => response.error.expect("uncertain response"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::OutcomeUnknown);
    harness
        .wait_for(|status| status.session_generation >= 2 && status.connected)
        .await;
    assert_eq!(
        shared.changes.lock().expect("changes").len(),
        1,
        "uncertain mutation was replayed"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_and_absent_adapter_states_reconcile_without_restarting_the_service() {
    let shared = Arc::new(Shared::default());
    shared.absent.store(true, Ordering::SeqCst);
    let harness = Harness::start(shared.clone(), Config::default()).await;
    harness
        .wait_for(|status| {
            status
                .last_error
                .as_ref()
                .is_some_and(|error| error.code == ErrorCode::AdapterUnavailable)
        })
        .await;
    shared.absent.store(false, Ordering::SeqCst);
    harness.ready().await;
    let previous_connections = shared.connections.load(Ordering::SeqCst);
    shared.sleeping.store(true, Ordering::SeqCst);
    harness
        .wait_for(|status| status.lifecycle == "suspended" && !status.connected)
        .await;
    shared.sleeping.store(false, Ordering::SeqCst);
    harness.ready().await;
    assert!(shared.connections.load(Ordering::SeqCst) > previous_connections);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_packets_and_stalled_clients_do_not_block_other_clients() {
    let harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    harness.ready().await;
    let peer = harness
        .shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("connected peer")
        .clone();
    for _ in 0..100 {
        peer.push(vec![4, 0, 4]);
    }
    harness
        .wait_for(|status| status.malformed_frames >= 100)
        .await;
    let mut stalled = UnixStream::connect(harness.paths.socket())
        .await
        .expect("stall connection");
    stalled
        .write_all(b"{\"version\":")
        .await
        .expect("partial request");
    let response = harness.request(30, Operation::AudioStatus, 1000).await;
    assert!(response.error.is_none());
    let mut bad = UnixStream::connect(harness.paths.socket())
        .await
        .expect("bad connection");
    bad.write_all(b"{\"version\":999,\"id\":1,\"operation\":{\"type\":\"status\"}}\n")
        .await
        .expect("bad request");
    let bytes = runtime::read_frame(&mut bad)
        .await
        .expect("error frame")
        .expect("response");
    let response: Response = serde_json::from_slice(&bytes).expect("typed error");
    assert_eq!(
        response.error.expect("version error").code,
        ErrorCode::VersionMismatch
    );
    let denied = harness
        .request(31, Operation::SetOutput("unrelated".into()), 1000)
        .await;
    assert_eq!(
        denied.error.expect("unsupported mutation").code,
        ErrorCode::Unsupported
    );
    drop(stalled);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preferences_persist_and_legacy_publication_cleans_up_on_shutdown() {
    let config = Config {
        legacy_compatibility: true,
        ..Config::default()
    };
    let harness = Harness::start(Arc::new(Shared::default()), config).await;
    harness.ready().await;
    let response = harness
        .request(40, Operation::EarPolicy(EarPolicy::BothRemoved), 1000)
        .await;
    assert!(response.error.is_none());
    assert_eq!(
        harness
            .paths
            .load_config()
            .expect("saved config")
            .ear_policy,
        EarPolicy::BothRemoved
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while !harness.paths.legacy_state.exists() {
        assert!(Instant::now() < deadline, "legacy status not published");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let legacy = harness.paths.legacy_state.clone();
    harness.stop().await;
    assert!(!legacy.exists(), "owned legacy snapshot survives stop");
}

#[test]
fn actual_serializer_generates_legacy_consumer_fixtures() {
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/compat-fixtures");
    std::fs::create_dir_all(&output).expect("fixture directory");
    let mut state = DeviceState::default();
    let fresh = Snapshot::new("fixture-instance".into(), &state);
    std::fs::write(
        output.join("fresh.json"),
        serde_json::to_vec_pretty(&fresh.legacy()).expect("fresh JSON"),
    )
    .expect("fresh fixture");
    state.set_device_identity(ADDRESS, "Example AirPods");
    state.begin_session(1);
    for bytes in [
        metadata(),
        battery_left(),
        packets::build_noise_control(NoiseControlMode::NoiseCancellation).to_vec(),
    ] {
        state.apply_packet(&parser::parse(&bytes).expect("fixture packet"), 1, 1);
    }
    let mut connected = Snapshot::new("fixture-instance".into(), &state);
    connected.connected = true;
    assert_eq!(connected.device.model_number, "A3048");
    assert_eq!(connected.device.listening_mode, Some(NoiseControlMode::NoiseCancellation));
    assert_eq!(connected.device.battery.left.level, 100);
    std::fs::write(
        output.join("pro2.json"),
        serde_json::to_vec_pretty(&connected.legacy()).expect("connected JSON"),
    )
    .expect("connected fixture");
    state.end_session(1);
    let disconnected = Snapshot::new("fixture-instance".into(), &state);
    std::fs::write(
        output.join("disconnected.json"),
        serde_json::to_vec_pretty(&disconnected.legacy()).expect("disconnected JSON"),
    )
    .expect("disconnected fixture");
}
