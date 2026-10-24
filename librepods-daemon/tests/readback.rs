// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

#![cfg(target_os = "linux")]

use librepods_core::{
    protocol::{control_command::ControlCommand, packets, parser, settings::SettingChange},
    transport::TransportError,
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
const ADDRESS: &str = "00:11:22:33:44:55";
#[derive(Default)]
struct Facts {
    connections: AtomicUsize,
    mutations: Mutex<Vec<Vec<u8>>>,
    value: AtomicUsize,
    wrong_endpoint: bool,
    wrong_model: bool,
    suspend_on_write: bool,
    stall_reopen: bool,
    sleeping: AtomicBool,
    connect_budgets: Mutex<Vec<Duration>>,
    send_budgets: Mutex<Vec<(usize, Duration)>>,
}
struct Fake(Arc<Facts>);
impl Backend for Fake {
    fn discover(&self, address: Option<&str>) -> Result<Endpoint, DaemonError> {
        assert_eq!(address, Some(ADDRESS));
        Ok(Endpoint {
            address: if self.0.wrong_endpoint && !self.0.mutations.lock().unwrap().is_empty() {
                "00:11:22:33:44:66"
            } else {
                ADDRESS
            }
            .into(),
            name: "Example AirPods".into(),
        })
    }
    fn connect(
        &self,
        _: &Endpoint,
        budget: Duration,
    ) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        self.0.connect_budgets.lock().unwrap().push(budget);
        let ordinal = self.0.connections.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Arc::new(Peer {
            facts: self.0.clone(),
            ordinal,
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
        }))
    }
    fn sleeping(&self) -> bool {
        self.0.sleeping.load(Ordering::SeqCst)
    }
}
struct Peer {
    facts: Arc<Facts>,
    ordinal: usize,
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
    fn send(&self, packet: &[u8], budget: Duration) -> Result<(), TransportError> {
        self.facts
            .send_budgets
            .lock()
            .unwrap()
            .push((self.ordinal, budget));
        if packet == packets::HANDSHAKE {
            if self.ordinal == 1 || !self.facts.stall_reopen {
                self.push(packets::HANDSHAKE_ACK_PREFIX.to_vec());
            }
        } else if packet == packets::SET_SPECIFIC_FEATURES {
            self.push(packets::FEATURES_ACK_PREFIX.to_vec());
        } else if packet == packets::REQUEST_NOTIFICATIONS {
            let mut metadata = vec![4, 0, 4, 0, 0x1d, 0, 2, 0xd5, 0, 4, 0];
            metadata.extend_from_slice(if self.ordinal > 1 && self.facts.wrong_model {
                b"Example AirPods\0A2698\0Apple Inc.\0"
            } else {
                b"Example AirPods\0A3048\0Apple Inc.\0"
            });
            metadata.extend_from_slice(b"SANITIZED\0Firmware\0\xff\x80\0");
            // Deliberately report the setting before identity on the new channel.
            if self.ordinal > 1 {
                self.push(
                    ControlCommand::create(
                        0x28,
                        &[self.facts.value.load(Ordering::SeqCst) as u8, 0, 0, 0],
                    )
                    .to_vec(),
                );
            }
            self.push(metadata);
            if self.ordinal == 1 {
                self.push(ControlCommand::disabled(0x28).to_vec());
            }
        } else if matches!(
            parser::parse(packet),
            Ok(parser::ParsedPacket::ControlCommand { .. })
        ) {
            self.facts.mutations.lock().unwrap().push(packet.to_vec());
            self.facts
                .value
                .store(usize::from(packet[7]), Ordering::SeqCst);
            if self.facts.suspend_on_write {
                self.facts.sleeping.store(true, Ordering::SeqCst);
            }
            // No same-generation echo: only the next startup snapshot can verify it.
        }
        Ok(())
    }
    fn recv(&self, bytes: &mut [u8], budget: Duration) -> Result<usize, TransportError> {
        let queue = self.queue.lock().unwrap();
        let (mut queue, _) = self
            .wake
            .wait_timeout_while(queue, budget, |queue| {
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
fn start(facts: Arc<Facts>) -> Session {
    Session::spawn(
        Arc::new(Fake(facts)),
        Config {
            address: Some(ADDRESS.into()),
            ..Config::default()
        },
        "readback-test".into(),
    )
}
async fn wait(
    status: &mut watch::Receiver<Snapshot>,
    predicate: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = status.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("readback readiness")
}
async fn baseline(session: &Session) {
    wait(&mut session.handle.status.clone(), |s| {
        s.connected && s.device.reported_controls.get(&0x28) == Some(&[2, 0, 0, 0])
    })
    .await;
}
fn change(timeout_ms: u64) -> Request {
    Request {
        version: 1,
        id: 10,
        timeout_ms,
        operation: Operation::Change(SettingChange::ConversationalAwareness(true)),
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_mutation_confirms_only_after_new_identity_and_setting_on_the_same_device() {
    let facts = Arc::new(Facts::default());
    let session = start(facts.clone());
    baseline(&session).await;
    let generation = session.handle.status.borrow().session_generation;
    let response = session.handle.request(change(1500)).await;
    assert!(response.error.is_none(), "{response:?}");
    let snapshot = session.handle.status.borrow().clone();
    assert!(snapshot.session_generation > generation);
    assert_eq!(
        snapshot.device.observations["metadata"].generation,
        snapshot.session_generation
    );
    assert_eq!(
        snapshot.device.observations["control.28"].generation,
        snapshot.session_generation
    );
    assert_eq!(facts.mutations.lock().unwrap().len(), 1);
    assert_eq!(facts.connections.load(Ordering::SeqCst), 2);
    session.stop();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_endpoint_or_new_model_cannot_verify_the_written_setting() {
    for wrong_endpoint in [false, true] {
        let facts = Arc::new(Facts {
            wrong_endpoint,
            wrong_model: !wrong_endpoint,
            ..Facts::default()
        });
        let session = start(facts.clone());
        baseline(&session).await;
        let response = session.handle.request(change(1000)).await;
        assert_eq!(response.error.unwrap().code, ErrorCode::OutcomeUnknown);
        assert_eq!(facts.mutations.lock().unwrap().len(), 1);
        assert_eq!(
            facts.connections.load(Ordering::SeqCst),
            if wrong_endpoint { 1 } else { 2 }
        );
        session.stop();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspend_after_the_single_write_cancels_verification_without_replaying_on_resume() {
    let facts = Arc::new(Facts {
        suspend_on_write: true,
        ..Facts::default()
    });
    let session = start(facts.clone());
    baseline(&session).await;
    let response = session.handle.request(change(1000)).await;
    assert_eq!(response.error.unwrap().code, ErrorCode::OutcomeUnknown);
    wait(&mut session.handle.status.clone(), |s| {
        s.lifecycle == "suspended"
    })
    .await;
    assert_eq!(facts.connections.load(Ordering::SeqCst), 1);
    facts.sleeping.store(false, Ordering::SeqCst);
    wait(&mut session.handle.status.clone(), |s| {
        s.connected && s.device.reported_controls.get(&0x28) == Some(&[1, 0, 0, 0])
    })
    .await;
    assert_eq!(facts.mutations.lock().unwrap().len(), 1);
    session.stop();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_and_handshake_budgets_are_capped_by_the_original_request_deadline() {
    let facts = Arc::new(Facts {
        stall_reopen: true,
        ..Facts::default()
    });
    let session = start(facts.clone());
    baseline(&session).await;
    let started = Instant::now();
    let response = session.handle.request(change(150)).await;
    assert_eq!(response.error.unwrap().code, ErrorCode::OutcomeUnknown);
    assert!(started.elapsed() < Duration::from_millis(600));
    assert!(facts.connect_budgets.lock().unwrap()[1] <= Duration::from_millis(150));
    assert!(facts
        .send_budgets
        .lock()
        .unwrap()
        .iter()
        .filter(|(ordinal, _)| *ordinal == 2)
        .all(|(_, budget)| *budget <= Duration::from_millis(150)));
    assert_eq!(facts.mutations.lock().unwrap().len(), 1);
    session.stop();
}
