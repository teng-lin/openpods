// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

#![cfg(target_os = "linux")]

use librepods_core::{protocol::packets, transport::TransportError};
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
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};
use tokio::sync::watch;

const ADDRESS: &str = "00:11:22:33:44:55";
#[derive(Default)]
struct Facts {
    availability: AtomicU8, // 0 connected, 1 adapter absent, 2 pods absent
    failure: AtomicU8,      // 0 succeeds, 1 transient, 2 permission denied
    sleeping: AtomicBool,
    stalled_handshake: AtomicBool,
    connections: AtomicUsize,
}
struct Fake(Arc<Facts>);
impl Backend for Fake {
    fn discover(&self, _: Option<&str>) -> Result<Endpoint, DaemonError> {
        match self.0.availability.load(Ordering::SeqCst) {
            1 => Err(DaemonError::transient(
                ErrorCode::AdapterUnavailable,
                "discovery",
                "adapter absent",
            )),
            2 => Err(DaemonError::transient(
                ErrorCode::DeviceUnavailable,
                "discovery",
                "pods absent",
            )),
            _ => Ok(Endpoint {
                address: ADDRESS.into(),
                name: "Example AirPods".into(),
            }),
        }
    }
    fn connect(&self, _: &Endpoint, _: Duration) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        self.0.connections.fetch_add(1, Ordering::SeqCst);
        match self.0.failure.load(Ordering::SeqCst) {
            1 => Err(DaemonError::transient(
                ErrorCode::ConnectFailed,
                "connect",
                "control channel unavailable",
            )),
            2 => Err(DaemonError::new(
                ErrorCode::PermissionDenied,
                "connect",
                "permission denied",
            )),
            _ => Ok(Arc::new(Peer {
                facts: self.0.clone(),
                queue: Mutex::new(VecDeque::new()),
                wake: Condvar::new(),
                closed: AtomicBool::new(false),
            })),
        }
    }
    fn sleeping(&self) -> bool {
        self.0.sleeping.load(Ordering::SeqCst)
    }
}
struct Peer {
    facts: Arc<Facts>,
    queue: Mutex<VecDeque<Vec<u8>>>,
    wake: Condvar,
    closed: AtomicBool,
}
impl Peer {
    fn push(&self, packet: Vec<u8>) {
        self.queue.lock().unwrap().push_back(packet);
        self.wake.notify_one();
    }
}
impl SessionTransport for Peer {
    fn send(&self, bytes: &[u8], _: Duration) -> Result<(), TransportError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(TransportError::ConnectionClosed);
        }
        if bytes == packets::HANDSHAKE && !self.facts.stalled_handshake.load(Ordering::SeqCst) {
            self.push(packets::HANDSHAKE_ACK_PREFIX.to_vec());
        } else if bytes == packets::SET_SPECIFIC_FEATURES {
            self.push(packets::FEATURES_ACK_PREFIX.to_vec());
        } else if bytes == packets::REQUEST_NOTIFICATIONS {
            let mut metadata = vec![4, 0, 4, 0, 0x1d, 0, 2, 0xd5, 0, 4, 0];
            metadata.extend_from_slice(
                b"Example AirPods\0A3048\0Apple Inc.\0SANITIZED\0Firmware\0\xff\x80\0",
            );
            self.push(metadata);
        }
        Ok(())
    }
    fn recv(&self, bytes: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        let queue = self.queue.lock().unwrap();
        let (mut queue, _) = self
            .wake
            .wait_timeout_while(queue, timeout, |queue| {
                queue.is_empty() && !self.closed.load(Ordering::SeqCst)
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
        "recovery-test".into(),
    )
}
async fn wait(
    status: &mut watch::Receiver<Snapshot>,
    predicate: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = status.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("session recovery deadline")
}
async fn ready(status: &mut watch::Receiver<Snapshot>) -> Snapshot {
    wait(status, |s| s.connected && s.device.model_number == "A3048").await
}
async fn reconnect(session: &Session) {
    let response = session
        .handle
        .request(Request {
            version: 1,
            id: 1,
            timeout_ms: 1000,
            operation: Operation::Reconnect,
        })
        .await;
    assert!(response.error.is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connected_control_failure_uses_first_probe_budget_and_then_resets_on_ready() {
    let facts = Arc::new(Facts::default());
    facts.failure.store(1, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    let failed = wait(&mut status, |s| {
        s.lifecycle == "recovering" && s.retry_count == 1
    })
    .await;
    let delay = failed
        .retry_at_ms
        .unwrap()
        .saturating_sub(failed.observed_monotonic_ms);
    assert!((700..=1250).contains(&delay));
    assert!(!failed.connected);
    facts.failure.store(0, Ordering::SeqCst);
    let ready = ready(&mut status).await;
    assert_eq!(ready.retry_count, 0);
    assert_eq!(ready.retry_at_ms, None);
    assert_eq!(facts.connections.load(Ordering::SeqCst), 2);
    session.stop();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adapter_loss_and_reconnect_during_suspend_do_not_consume_or_escape_budgets() {
    let facts = Arc::new(Facts::default());
    facts.availability.store(1, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    let missing = wait(&mut status, |s| {
        s.lifecycle == "waiting_for_adapter" && s.last_error.is_some()
    })
    .await;
    assert_eq!(missing.retry_count, 0);
    assert_eq!(facts.connections.load(Ordering::SeqCst), 0);
    facts.availability.store(0, Ordering::SeqCst);
    let previous = ready(&mut status).await.session_generation;
    facts.sleeping.store(true, Ordering::SeqCst);
    wait(&mut status, |s| s.lifecycle == "suspended").await;
    reconnect(&session).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(facts.connections.load(Ordering::SeqCst), 1);
    facts.sleeping.store(false, Ordering::SeqCst);
    assert!(ready(&mut status).await.session_generation > previous);
    session.stop();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_failure_requires_explicit_reconnect_after_correction() {
    let facts = Arc::new(Facts::default());
    facts.failure.store(2, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    wait(&mut status, |s| {
        s.lifecycle == "waiting_for_device"
            && s.last_error
                .as_ref()
                .is_some_and(|e| e.code == ErrorCode::PermissionDenied)
    })
    .await;
    facts.failure.store(0, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert_eq!(facts.connections.load(Ordering::SeqCst), 1);
    reconnect(&session).await;
    ready(&mut status).await;
    assert_eq!(facts.connections.load(Ordering::SeqCst), 2);
    session.stop();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_pods_exhaust_separate_budget_without_reclaiming_a_bluetooth_connection() {
    let facts = Arc::new(Facts::default());
    facts.availability.store(2, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    let absent = wait(&mut status, |s| {
        s.lifecycle == "waiting_for_device" && s.retry_count == 3 && s.retry_at_ms.is_none()
    })
    .await;
    assert!(!absent.connected);
    assert_eq!(facts.connections.load(Ordering::SeqCst), 0);
    facts.availability.store(0, Ordering::SeqCst);
    ready(&mut status).await;
    assert_eq!(facts.connections.load(Ordering::SeqCst), 1);
    session.stop();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspend_interrupts_an_incomplete_handshake_and_requires_a_fresh_generation() {
    let facts = Arc::new(Facts::default());
    facts.stalled_handshake.store(true, Ordering::SeqCst);
    let session = session(facts.clone());
    let mut status = session.handle.status.clone();
    let previous = wait(&mut status, |s| s.lifecycle == "awaiting_handshake")
        .await
        .session_generation;
    facts.sleeping.store(true, Ordering::SeqCst);
    wait(&mut status, |s| s.lifecycle == "suspended" && !s.connected).await;
    facts.stalled_handshake.store(false, Ordering::SeqCst);
    facts.sleeping.store(false, Ordering::SeqCst);
    assert!(ready(&mut status).await.session_generation > previous);
    session.stop();
}
