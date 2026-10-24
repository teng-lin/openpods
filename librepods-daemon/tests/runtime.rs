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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
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
    // Real firmware appends additional fields and opaque extension bytes.
    bytes.extend_from_slice(b"ExampleFirmware\0");
    bytes.extend_from_slice(&[1, 2, 3, 0x80]);
    bytes
}

fn battery_left() -> Vec<u8> {
    // A single left component reports a genuine 100% discharging reading.
    vec![4, 0, 4, 0, 4, 0, 1, 4, 1, 100, 2, 1]
}

fn key_packet(kind: u8, byte: u8) -> Vec<u8> {
    // Synthetic one-key TLV responses; these bytes never came from a device.
    let mut packet = vec![4, 0, 4, 0, 0x31, 0, 1, kind, 0, 16, 0];
    packet.extend_from_slice(&[byte; 16]);
    packet
}

#[derive(Default)]
struct Shared {
    sleeping: AtomicBool,
    absent: AtomicBool,
    lose_ack: AtomicBool,
    no_handshake: AtomicBool,
    slow_audio: AtomicBool,
    key_reply: AtomicU8,
    key_requests: AtomicUsize,
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
        } else if bytes == packets::REQUEST_MAGIC_CLOUD_KEYS {
            self.shared.key_requests.fetch_add(1, Ordering::SeqCst);
            let reply = self.shared.key_reply.load(Ordering::SeqCst);
            if reply > 0 {
                self.push(key_packet(1, 0x11));
            }
            if reply == 1 {
                self.push(key_packet(4, 0x22));
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

    fn audio_async(
        &self,
        address: String,
        operation: Operation,
    ) -> librepods_daemon::backend::AudioFuture {
        let delayed = self.0.slow_audio.load(Ordering::SeqCst);
        let result = self.audio(&address, &operation);
        Box::pin(async move {
            if delayed {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            result
        })
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
        Self::start_seeded(shared, config, None).await
    }

    async fn start_seeded(
        shared: Arc<Shared>,
        config: Config,
        legacy: Option<serde_json::Value>,
    ) -> Self {
        let directory = TempDir::new().expect("test directory");
        let root = directory.path();
        let paths = Paths {
            runtime: root.join("run/librepods-rs"),
            state: root.join("state/librepods-rs"),
            config: root.join("config/librepods-rs/config.json"),
            legacy_state: root.join("state/librepods/status.json"),
            legacy_socket: root.join("run/librepods.sock"),
        };
        if let Some(legacy) = legacy {
            librepods_daemon::storage::atomic_write(
                &paths.legacy_state,
                &serde_json::to_vec(&legacy).expect("seeded legacy JSON"),
            )
            .expect("seeded legacy state");
        }
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
            assert!(
                Instant::now() < deadline,
                "status condition did not become true: {diagnostic}; server finished={}",
                self.task.as_ref().is_some_and(|task| task.is_finished())
            );
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
        self.shutdown_in_place().await;
    }

    async fn shutdown_in_place(&mut self) {
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
    let mut harness = Harness::start(Arc::new(Shared::default()), config).await;
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
    harness.shutdown_in_place().await;
    assert!(
        !harness.paths.legacy_state.exists(),
        "owned legacy snapshot survives stop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_handshake_is_bounded_and_an_existing_owner_is_preserved() {
    let shared = Arc::new(Shared::default());
    shared.no_handshake.store(true, Ordering::SeqCst);
    let harness = Harness::start(shared, Config::default()).await;
    harness
        .wait_for(|status| status.lifecycle == "awaiting_handshake")
        .await;
    let duplicate = tokio::time::timeout(
        Duration::from_secs(1),
        runtime::run(
            harness.paths.clone(),
            Config::default(),
            Arc::new(EmulatedBackend(Arc::new(Shared::default()))),
            async {},
        ),
    )
    .await
    .expect("duplicate refusal");
    assert_eq!(
        duplicate.expect_err("owner already running").code,
        ErrorCode::ServiceUnavailable
    );
    assert!(
        harness.paths.socket().exists(),
        "rejected owner removed the active socket"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_stream_tracks_real_updates_and_rejects_oversized_requests() {
    let harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    harness.ready().await;
    let mut watcher = UnixStream::connect(harness.paths.socket())
        .await
        .expect("watch connection");
    let mut bytes = serde_json::to_vec(&Request {
        version: 1,
        id: 60,
        timeout_ms: 1000,
        operation: Operation::Watch,
    })
    .expect("watch JSON");
    bytes.push(b'\n');
    watcher.write_all(&bytes).await.expect("watch request");
    let first = runtime::read_frame(&mut watcher)
        .await
        .expect("snapshot frame")
        .expect("first snapshot");
    let first: Response = serde_json::from_slice(&first).expect("response");
    let first: Snapshot =
        serde_json::from_value(first.result.expect("snapshot")).expect("snapshot schema");
    let peer = harness
        .shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("peer")
        .clone();
    peer.push(ControlCommand::create(0x3E, &[2, 0, 0, 0]).to_vec());
    let next = tokio::time::timeout(Duration::from_secs(1), runtime::read_frame(&mut watcher))
        .await
        .expect("watch update")
        .expect("frame")
        .expect("updated snapshot");
    let next: Response = serde_json::from_slice(&next).expect("response");
    let next: Snapshot =
        serde_json::from_value(next.result.expect("snapshot")).expect("snapshot schema");
    assert!(next.revision > first.revision);
    assert_eq!(
        next.device.reported_controls.get(&0x3E),
        Some(&[2, 0, 0, 0])
    );
    let mut oversized = UnixStream::connect(harness.paths.socket())
        .await
        .expect("oversized connection");
    oversized
        .write_all(&vec![b' '; librepods_daemon::ipc::MAX_FRAME + 1])
        .await
        .expect("oversized request");
    let reply = runtime::read_frame(&mut oversized)
        .await
        .expect("error frame")
        .expect("error response");
    let reply: Response = serde_json::from_slice(&reply).expect("typed error");
    assert_eq!(
        reply.error.expect("oversize refusal").code,
        ErrorCode::InvalidInput
    );
    drop(watcher);
    harness.stop().await;
}

async fn wait_for_change_count(shared: &Shared, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while shared.changes.lock().expect("changes").len() < count {
        assert!(
            Instant::now() < deadline,
            "expected mutation was not dispatched"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_queued_requests_are_never_written_later() {
    let shared = Arc::new(Shared::default());
    let harness = Harness::start(shared.clone(), Config::default()).await;
    harness.ready().await;
    shared.lose_ack.store(true, Ordering::SeqCst);
    let paths = harness.paths.clone();
    let active = tokio::spawn(async move {
        runtime::request(
            &paths,
            &Request {
                version: 1,
                id: 70,
                timeout_ms: 1000,
                operation: Operation::Change(SettingChange::ListeningMode(
                    NoiseControlMode::Transparency,
                )),
            },
        )
        .await
    });
    wait_for_change_count(&shared, 1).await;
    let queued = runtime::request(
        &harness.paths,
        &Request {
            version: 1,
            id: 71,
            timeout_ms: 100,
            operation: Operation::Change(SettingChange::ConversationalAwareness(false)),
        },
    )
    .await;
    assert!(queued.is_err() || queued.expect("response").error.is_some());
    shared.lose_ack.store(false, Ordering::SeqCst);
    shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("peer")
        .push(packets::build_noise_control(NoiseControlMode::Transparency).to_vec());
    assert!(active
        .await
        .expect("client task")
        .expect("active response")
        .error
        .is_none());
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        shared.changes.lock().expect("changes").len(),
        1,
        "expired queued mutation was written"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_queued_client_cannot_cause_a_late_mutation() {
    let shared = Arc::new(Shared::default());
    let harness = Harness::start(shared.clone(), Config::default()).await;
    harness.ready().await;
    shared.lose_ack.store(true, Ordering::SeqCst);
    let paths = harness.paths.clone();
    let active = tokio::spawn(async move {
        runtime::request(
            &paths,
            &Request {
                version: 1,
                id: 80,
                timeout_ms: 1000,
                operation: Operation::Change(SettingChange::ListeningMode(
                    NoiseControlMode::Transparency,
                )),
            },
        )
        .await
    });
    wait_for_change_count(&shared, 1).await;
    let mut queued = UnixStream::connect(harness.paths.socket())
        .await
        .expect("queued client");
    let mut bytes = serde_json::to_vec(&Request {
        version: 1,
        id: 81,
        timeout_ms: 1000,
        operation: Operation::Change(SettingChange::ConversationalAwareness(false)),
    })
    .expect("queued JSON");
    bytes.push(b'\n');
    queued.write_all(&bytes).await.expect("queued request");
    tokio::time::sleep(Duration::from_millis(80)).await;
    drop(queued);
    shared.lose_ack.store(false, Ordering::SeqCst);
    shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("peer")
        .push(packets::build_noise_control(NoiseControlMode::Transparency).to_vec());
    assert!(active
        .await
        .expect("client task")
        .expect("active response")
        .error
        .is_none());
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        shared.changes.lock().expect("changes").len(),
        1,
        "disconnected queued client caused a write"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_after_write_recovers_without_replaying_the_uncertain_mutation() {
    let shared = Arc::new(Shared::default());
    let harness = Harness::start(shared.clone(), Config::default()).await;
    let initial = harness.ready().await;
    shared.lose_ack.store(true, Ordering::SeqCst);
    let mut client = UnixStream::connect(harness.paths.socket())
        .await
        .expect("client");
    let mut bytes = serde_json::to_vec(&Request {
        version: 1,
        id: 82,
        timeout_ms: 1000,
        operation: Operation::Change(SettingChange::ListeningMode(NoiseControlMode::Transparency)),
    })
    .expect("request");
    bytes.push(b'\n');
    client.write_all(&bytes).await.expect("request write");
    wait_for_change_count(&shared, 1).await;
    drop(client);
    // Once written, cancellation cannot promise that the change did not apply.
    // The fake peer applied it but withheld its ACK. Reconnection must observe
    // the changed mode, never resend the cancelled command.
    let recovered = harness
        .wait_for(|snapshot| {
            snapshot.connected
                && snapshot.session_generation > initial.session_generation
                && snapshot.device.listening_mode == Some(NoiseControlMode::Transparency)
        })
        .await;
    let observed = &recovered.device.observations["noise_control"];
    assert_eq!(observed.generation, recovered.session_generation);
    assert!(!observed.stale);
    assert_eq!(
        shared.changes.lock().expect("changes").len(),
        1,
        "cancelled dispatched mutation was replayed"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_read_timeout_is_certain_while_audio_mutation_timeout_is_uncertain() {
    let shared = Arc::new(Shared::default());
    shared.slow_audio.store(true, Ordering::SeqCst);
    let session = librepods_daemon::session::Session::spawn(
        Arc::new(EmulatedBackend(shared)),
        Config::default(),
        "audio-timeouts".into(),
    );
    for (operation, expected) in [
        (Operation::AudioStatus, ErrorCode::CommandTimeout),
        (
            Operation::SetInput("example-source".into()),
            ErrorCode::OutcomeUnknown,
        ),
    ] {
        let response = session
            .handle
            .request(Request {
                version: 1,
                id: 83,
                timeout_ms: 100,
                operation,
            })
            .await;
        assert_eq!(response.error.expect("timed-out audio").code, expected);
    }
    session.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_preserves_an_unexpected_file_replacing_the_socket() {
    let mut harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    harness.ready().await;
    let socket = harness.paths.socket();
    std::fs::remove_file(&socket).expect("remove test socket pathname");
    std::fs::write(&socket, b"unexpected replacement").expect("replacement file");
    harness
        .shutdown
        .take()
        .expect("shutdown sender")
        .send(())
        .expect("shutdown request");
    harness
        .task
        .take()
        .expect("server task")
        .await
        .expect("server join")
        .expect("server result");
    assert_eq!(
        std::fs::read(&socket).expect("unowned replacement must survive"),
        b"unexpected replacement"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_request_timeout_is_not_reported_as_an_uncertain_mutation() {
    let directory = TempDir::new().expect("test directory");
    let paths = Paths {
        runtime: directory.path().join("run"),
        state: directory.path().join("state"),
        config: directory.path().join("config.json"),
        legacy_state: directory.path().join("legacy.json"),
        legacy_socket: directory.path().join("legacy.sock"),
    };
    std::fs::create_dir(&paths.runtime).expect("runtime directory");
    let listener = tokio::net::UnixListener::bind(paths.socket()).expect("unresponsive server");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("client");
        let _ = runtime::read_frame(&mut stream).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    let error = runtime::request(
        &paths,
        &Request {
            version: 1,
            id: 90,
            timeout_ms: 100,
            operation: Operation::Status,
        },
    )
    .await
    .expect_err("read timeout");
    assert_eq!(
        error.code,
        ErrorCode::CommandTimeout,
        "a read-only request cannot have mutated the device"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_failure_is_visible_without_breaking_control() {
    let harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    let settled = harness
        .wait_for(|snapshot| snapshot.device.reported_controls.contains_key(&0x1B))
        .await;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let published = std::fs::read(harness.paths.state_file())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok());
        if published.is_some_and(|snapshot| snapshot.revision == settled.revision) {
            break;
        }
        assert!(Instant::now() < deadline, "initial publication not settled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    std::fs::remove_file(harness.paths.state_file()).expect("remove test snapshot");
    std::fs::create_dir(harness.paths.state_file()).expect("inject rename failure");
    harness
        .shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("peer")
        .push(ControlCommand::create(0x3E, &[2, 0, 0, 0]).to_vec());
    let degraded = harness
        .wait_for(|snapshot| snapshot.publication_error.is_some())
        .await;
    assert!(degraded.connected, "publication failure broke AACP");
    std::fs::remove_dir(harness.paths.state_file()).expect("remove injected obstacle");
    let response = harness
        .request(
            91,
            Operation::Change(SettingChange::ListeningMode(NoiseControlMode::Transparency)),
            1000,
        )
        .await;
    assert!(
        response.error.is_none(),
        "control failed during publication degradation"
    );
    harness
        .wait_for(|snapshot| snapshot.publication_error.is_none())
        .await;
    harness.stop().await;
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
    assert_eq!(
        connected.device.listening_mode,
        Some(NoiseControlMode::NoiseCancellation)
    );
    assert_eq!(connected.device.battery.left.level, 100);
    std::fs::write(
        output.join("pro2.json"),
        serde_json::to_vec_pretty(&connected.legacy()).expect("connected JSON"),
    )
    .expect("connected fixture");
    let native = output
        .parent()
        .expect("target directory")
        .join("native-fixtures");
    std::fs::create_dir_all(&native).expect("native fixture directory");
    std::fs::write(
        native.join("pro2.json"),
        serde_json::to_vec_pretty(&connected).expect("native JSON"),
    )
    .expect("native connected fixture");
    state.end_session(1);
    let disconnected = Snapshot::new("fixture-instance".into(), &state);
    std::fs::write(
        native.join("disconnected.json"),
        serde_json::to_vec_pretty(&disconnected).expect("native JSON"),
    )
    .expect("native disconnected fixture");
    std::fs::write(
        output.join("disconnected.json"),
        serde_json::to_vec_pretty(&disconnected.legacy()).expect("disconnected JSON"),
    )
    .expect("disconnected fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newer_queued_setting_supersedes_old_desire_without_extra_writes() {
    let shared = Arc::new(Shared::default());
    let harness = Harness::start(shared.clone(), Config::default()).await;
    harness.ready().await;
    shared.lose_ack.store(true, Ordering::SeqCst);
    let paths = harness.paths.clone();
    let active = tokio::spawn(async move {
        runtime::request(
            &paths,
            &Request {
                version: 1,
                id: 100,
                timeout_ms: 2000,
                operation: Operation::Change(SettingChange::ListeningMode(
                    NoiseControlMode::Transparency,
                )),
            },
        )
        .await
    });
    wait_for_change_count(&shared, 1).await;
    let paths = harness.paths.clone();
    let older = tokio::spawn(async move {
        runtime::request(
            &paths,
            &Request {
                version: 1,
                id: 101,
                timeout_ms: 1500,
                operation: Operation::Change(SettingChange::ConversationalAwareness(false)),
            },
        )
        .await
    });
    harness.wait_for(|status| status.queue_depth >= 1).await;
    let paths = harness.paths.clone();
    let newer = tokio::spawn(async move {
        runtime::request(
            &paths,
            &Request {
                version: 1,
                id: 102,
                timeout_ms: 1500,
                operation: Operation::Change(SettingChange::ConversationalAwareness(true)),
            },
        )
        .await
    });
    let replaced = older.await.expect("older client").expect("response");
    assert_eq!(
        replaced.error.expect("superseded").code,
        ErrorCode::Superseded
    );
    shared.lose_ack.store(false, Ordering::SeqCst);
    shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("peer")
        .push(packets::build_noise_control(NoiseControlMode::Transparency).to_vec());
    assert!(active
        .await
        .expect("active")
        .expect("response")
        .error
        .is_none());
    let latest = newer.await.expect("newer").expect("response");
    assert_eq!(latest.result, Some(serde_json::json!("AlreadySet")));
    assert_eq!(shared.changes.lock().expect("changes").len(), 1);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_actor_queue_rejects_excess_and_expires_without_late_reconnects() {
    let shared = Arc::new(Shared::default());
    let session = librepods_daemon::session::Session::spawn(
        Arc::new(EmulatedBackend(shared.clone())),
        Config::default(),
        "queue-pressure".into(),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.handle.status.borrow().device.listening_mode
        != Some(NoiseControlMode::NoiseCancellation)
    {
        assert!(Instant::now() < deadline, "session did not become ready");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    shared.lose_ack.store(true, Ordering::SeqCst);
    let handle = session.handle.clone();
    let active = tokio::spawn(async move {
        handle
            .request(Request {
                version: 1,
                id: 200,
                timeout_ms: 2000,
                operation: Operation::Change(SettingChange::ListeningMode(
                    NoiseControlMode::Transparency,
                )),
            })
            .await
    });
    wait_for_change_count(&shared, 1).await;
    let mut clients = tokio::task::JoinSet::new();
    for id in 201..329 {
        let handle = session.handle.clone();
        clients.spawn(async move {
            handle
                .request(Request {
                    version: 1,
                    id,
                    timeout_ms: 200,
                    operation: Operation::Reconnect,
                })
                .await
        });
    }
    let mut rejected = 0;
    while let Some(result) = clients.join_next().await {
        let error = result
            .expect("client")
            .error
            .expect("bounded queue must not execute pending reconnects");
        match error.code {
            ErrorCode::QueueFull => rejected += 1,
            ErrorCode::CommandTimeout => {}
            code => panic!("unexpected overload result {code:?}"),
        }
    }
    assert!(rejected > 0, "queue had no enforced capacity");
    assert_eq!(shared.connections.load(Ordering::SeqCst), 1);
    assert_eq!(shared.changes.lock().expect("changes").len(), 1);
    active.abort();
    session.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_legacy_publication_and_never_recreates_owned_files_after_return() {
    // Keep the directory alive after shutdown; TempDir cleanup must not hide
    // delayed publisher writes or make a missing-artifact assertion vacuous.
    for _ in 0..8 {
        let mut harness = Harness::start(
            Arc::new(Shared::default()),
            Config {
                legacy_compatibility: true,
                ..Config::default()
            },
        )
        .await;
        harness.ready().await;
        let deadline = Instant::now() + Duration::from_secs(1);
        while !harness.paths.legacy_state.exists() {
            assert!(Instant::now() < deadline, "legacy publication missing");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let peer = harness
            .shared
            .peers
            .lock()
            .expect("peers")
            .last()
            .expect("peer")
            .clone();
        for value in 0..128 {
            peer.push(ControlCommand::create(0x3E, &[value, 0, 0, 0]).to_vec());
        }
        harness.shutdown_in_place().await;
        for _ in 0..6 {
            assert!(
                !harness.paths.socket().exists(),
                "socket recreated after shutdown"
            );
            assert!(
                !harness.paths.state_file().exists(),
                "native status recreated after shutdown"
            );
            assert!(
                !harness.paths.legacy_state.exists(),
                "legacy status recreated after shutdown"
            );
            assert!(
                harness.paths.runtime.is_dir(),
                "test directory vanished before assertion"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(not(feature = "ble"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_ble_feature_rejects_enabling_and_key_fetch_without_disturbing_aacp() {
    let harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    let initial = harness.ready().await;
    for operation in [Operation::BleEnabled(true), Operation::FetchKeys] {
        assert_eq!(
            harness
                .request(300, operation, 1000)
                .await
                .error
                .expect("unsupported BLE")
                .code,
            ErrorCode::Unsupported
        );
    }
    assert!(!harness.paths.load_config().expect("config").ble_enabled);
    assert_eq!(harness.shared.key_requests.load(Ordering::SeqCst), 0);
    assert!(!harness.paths.state.join("ble-keys.json").exists());
    let status = harness.ready().await;
    assert_eq!(status.session_generation, initial.session_generation);
    assert!(harness
        .request(
            301,
            Operation::Change(SettingChange::SingleEarbudAnc(true)),
            1000
        )
        .await
        .error
        .is_none());
    harness.stop().await;
}

#[cfg(feature = "ble")]
async fn wait_for_key_requests(shared: &Shared, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while shared.key_requests.load(Ordering::SeqCst) < count {
        assert!(Instant::now() < deadline, "key request was not sent");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(feature = "ble")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_key_fetch_waits_for_both_new_packets_persists_privately_and_forgets() {
    use std::os::unix::fs::PermissionsExt;
    let shared = Arc::new(Shared::default());
    shared.key_reply.store(2, Ordering::SeqCst); // Withhold encryption key.
    let harness = Harness::start(shared.clone(), Config::default()).await;
    let initial = harness.ready().await;
    let paths = harness.paths.clone();
    let fetch = tokio::spawn(async move {
        runtime::request(
            &paths,
            &Request {
                version: 1,
                id: 310,
                timeout_ms: 1500,
                operation: Operation::FetchKeys,
            },
        )
        .await
    });
    wait_for_key_requests(&shared, 1).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        !fetch.is_finished(),
        "one key incorrectly completed acquisition"
    );
    assert!(
        !harness.paths.state.join("ble-keys.json").exists(),
        "partial keys were persisted"
    );
    shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("peer")
        .push(key_packet(4, 0x22));
    let response = fetch.await.expect("fetch client").expect("response");
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(
        response.result,
        Some(serde_json::json!({"keys_available": true}))
    );
    let key_path = harness.paths.state.join("ble-keys.json");
    assert_eq!(
        std::fs::metadata(&key_path)
            .expect("private key store")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&key_path).expect("keys")).expect("private JSON");
    assert_eq!(
        stored["devices"][ADDRESS]["irk"],
        serde_json::json!(vec![0x11_u8; 16])
    );
    assert_eq!(
        stored["devices"][ADDRESS]["encryption_key"],
        serde_json::json!(vec![0x22_u8; 16])
    );
    let enabled = harness
        .request(311, Operation::BleEnabled(true), 1000)
        .await;
    assert!(enabled.error.is_none());
    let status = harness
        .wait_for(|status| status.ble["keys_available"] == true)
        .await;
    assert_eq!(status.session_generation, initial.session_generation);
    for encoded in [
        serde_json::to_string(&status).expect("public status"),
        status.legacy().to_string(),
    ] {
        assert!(!encoded.contains("\"irk\""));
        assert!(!encoded.contains("encryption_key"));
        assert!(!encoded.contains("magic_acc"));
    }
    let forgotten = harness.request(312, Operation::ForgetKeys, 1000).await;
    assert!(forgotten.error.is_none());
    assert!(librepods_daemon::keys::KeyStore::load(key_path)
        .expect("store")
        .get(ADDRESS)
        .expect("lookup")
        .is_none());
    harness
        .wait_for(|status| status.ble["keys_available"] == false && status.connected)
        .await;
    assert!(harness
        .request(313, Operation::BleEnabled(false), 1000)
        .await
        .error
        .is_none());
    assert!(
        !harness
            .paths
            .load_config()
            .expect("persisted BLE policy")
            .ble_enabled
    );
    harness.stop().await;
}

#[cfg(feature = "ble")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_key_request_times_out_without_saving_or_replaying_and_aacp_recovers() {
    let shared = Arc::new(Shared::default());
    shared.key_reply.store(2, Ordering::SeqCst);
    let harness = Harness::start(shared.clone(), Config::default()).await;
    let initial = harness.ready().await;
    let result = runtime::request(
        &harness.paths,
        &Request {
            version: 1,
            id: 320,
            timeout_ms: 100,
            operation: Operation::FetchKeys,
        },
    )
    .await;
    let error = match result {
        Ok(response) => response.error.expect("incomplete exchange"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::CommandTimeout);
    let old_peer = shared
        .peers
        .lock()
        .expect("peers")
        .first()
        .expect("old peer")
        .clone();
    harness
        .wait_for(|status| {
            status.connected && status.session_generation > initial.session_generation
        })
        .await;
    old_peer.push(key_packet(4, 0x22)); // A late response belongs to the closed session.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!harness.paths.state.join("ble-keys.json").exists());
    assert_eq!(
        shared.key_requests.load(Ordering::SeqCst),
        1,
        "expired key request replayed"
    );
    assert!(harness
        .request(
            321,
            Operation::Change(SettingChange::SingleEarbudAnc(true)),
            1000
        )
        .await
        .error
        .is_none());
    harness.stop().await;
}

#[cfg(feature = "ble")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_ble_keys_never_block_aacp_or_start_scanning_while_connected() {
    let harness = Harness::start(
        Arc::new(Shared::default()),
        Config {
            ble_enabled: true,
            ..Config::default()
        },
    )
    .await;
    let initial = harness.ready().await;
    let status = harness
        .wait_for(|status| status.ble["health"] == "paused")
        .await;
    assert_eq!(status.ble["keys_available"], false);
    assert_eq!(status.ble["connected_scanning"], false);
    assert_eq!(
        harness.shared.key_requests.load(Ordering::SeqCst),
        0,
        "keys fetched without explicit request"
    );
    assert!(harness
        .request(
            330,
            Operation::Change(SettingChange::ListeningMode(NoiseControlMode::Transparency)),
            1000
        )
        .await
        .error
        .is_none());
    assert_eq!(
        harness.ready().await.session_generation,
        initial.session_generation
    );
    harness.stop().await;
}

#[cfg(feature = "ble")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_key_fetch_discards_old_responses_and_requires_a_complete_new_exchange() {
    let shared = Arc::new(Shared::default());
    shared.key_reply.store(2, Ordering::SeqCst);
    let harness = Harness::start(shared.clone(), Config::default()).await;
    let initial = harness.ready().await;
    let mut client = UnixStream::connect(harness.paths.socket())
        .await
        .expect("client");
    let mut bytes = serde_json::to_vec(&Request {
        version: 1,
        id: 340,
        timeout_ms: 1000,
        operation: Operation::FetchKeys,
    })
    .expect("request");
    bytes.push(b'\n');
    client.write_all(&bytes).await.expect("key request");
    wait_for_key_requests(&shared, 1).await;
    let old = shared
        .peers
        .lock()
        .expect("peers")
        .first()
        .expect("old peer")
        .clone();
    drop(client);
    harness
        .wait_for(|status| {
            status.connected && status.session_generation > initial.session_generation
        })
        .await;
    old.push(key_packet(4, 0x22));
    let current = shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("new peer")
        .clone();
    current.push(key_packet(4, 0x22)); // Unsolicited cached key cannot complete a later request.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        shared.key_requests.load(Ordering::SeqCst),
        1,
        "cancelled exchange replayed"
    );
    assert!(!harness.paths.state.join("ble-keys.json").exists());
    let paths = harness.paths.clone();
    let fresh = tokio::spawn(async move {
        runtime::request(
            &paths,
            &Request {
                version: 1,
                id: 341,
                timeout_ms: 1000,
                operation: Operation::FetchKeys,
            },
        )
        .await
    });
    wait_for_key_requests(&shared, 2).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        !fresh.is_finished(),
        "cached encryption key incorrectly completed a new exchange"
    );
    current.push(key_packet(4, 0x22));
    assert!(fresh
        .await
        .expect("client task")
        .expect("response")
        .error
        .is_none());
    assert!(harness.paths.state.join("ble-keys.json").is_file());
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipelined_native_requests_keep_frames_and_identifiers_separate() {
    let harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    harness.ready().await;
    let mut client = UnixStream::connect(harness.paths.socket())
        .await
        .expect("client");
    let mut frames = b"invalid JSON\n".to_vec();
    for (id, operation) in [
        (350, Operation::Status),
        (351, Operation::Capabilities),
        (352, Operation::Doctor),
    ] {
        frames.extend(
            serde_json::to_vec(&Request {
                version: 1,
                id,
                timeout_ms: 1000,
                operation,
            })
            .expect("frame"),
        );
        frames.push(b'\n');
    }
    client.write_all(&frames).await.expect("pipelined write");
    let malformed: Response = serde_json::from_slice(
        &runtime::read_frame(&mut client)
            .await
            .expect("read")
            .expect("response"),
    )
    .expect("error schema");
    assert_eq!(malformed.id, 0);
    assert_eq!(
        malformed.error.expect("malformed request").code,
        ErrorCode::InvalidInput
    );
    for id in 350..353 {
        let response: Response = serde_json::from_slice(
            &runtime::read_frame(&mut client)
                .await
                .expect("read")
                .expect("response"),
        )
        .expect("response schema");
        assert_eq!(response.id, id);
        assert!(response.error.is_none(), "pipelined request failed");
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_legacy_publication_is_preserved_and_degradation_does_not_break_control() {
    let foreign = Snapshot::new("foreign-owner".into(), &DeviceState::default()).legacy();
    let mut harness = Harness::start_seeded(
        Arc::new(Shared::default()),
        Config {
            legacy_compatibility: true,
            ..Config::default()
        },
        Some(foreign.clone()),
    )
    .await;
    harness.ready().await;
    let degraded = harness
        .wait_for(|snapshot| snapshot.publication_error.is_some())
        .await;
    assert!(degraded.connected);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(&harness.paths.legacy_state).expect("foreign status")
        )
        .expect("legacy JSON"),
        foreign
    );
    assert!(
        harness
            .request(
                360,
                Operation::Change(SettingChange::ListeningMode(NoiseControlMode::Transparency)),
                1000
            )
            .await
            .error
            .is_none(),
        "legacy conflict blocked ANC"
    );
    harness.shutdown_in_place().await;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(&harness.paths.legacy_state)
                .expect("foreign status must survive shutdown")
        )
        .expect("legacy JSON"),
        foreign
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_one_byte_control_id_remains_observable_without_exceeding_native_frame_budget() {
    let harness = Harness::start(Arc::new(Shared::default()), Config::default()).await;
    harness.ready().await;
    let peer = harness
        .shared
        .peers
        .lock()
        .expect("peers")
        .last()
        .expect("peer")
        .clone();
    for id in 0..=u8::MAX {
        peer.push(ControlCommand::create(id, &[2, 0, 0, 0]).to_vec());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !peer.incoming.lock().expect("queue").is_empty() {
        assert!(
            Instant::now() < deadline,
            "device notification queue did not drain"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let response = runtime::request(
        &harness.paths,
        &Request {
            version: 1,
            id: 370,
            timeout_ms: 1000,
            operation: Operation::Status,
        },
    )
    .await
    .expect("finite control-ID map must fit the native IPC frame budget");
    assert!(
        response.error.is_none(),
        "status failed after valid control notifications"
    );
    let status: Snapshot =
        serde_json::from_value(response.result.expect("snapshot")).expect("status schema");
    assert_eq!(status.device.reported_controls.len(), 256);
    assert_eq!(status.device.reported_controls[&0xFE], [2, 0, 0, 0]);
    harness.stop().await;
}
