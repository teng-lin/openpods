// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{
    backend::{is_timeout, transport_error, Backend, SessionTransport},
    config::Config,
    ipc::{Operation, Request, Response},
    snapshot::{DeviceStatus, Snapshot},
    DaemonError, ErrorCode,
};
use librepods_core::{
    device::state::DeviceState,
    protocol::{
        handshake::{Handshake, HandshakeAction},
        parser,
        settings::{CommandAction, CommandOutcome, PendingChange},
    },
    transport::TransportError,
};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, watch};

const QUEUE_LIMIT: usize = 64;
const READ_SLICE: Duration = Duration::from_millis(40);

struct Envelope {
    request: Request,
    deadline: Instant,
    reply: oneshot::Sender<Response>,
}
struct Active {
    envelope: Envelope,
    pending: PendingChange,
}

#[derive(Clone)]
pub struct SessionHandle {
    sender: mpsc::SyncSender<Envelope>,
    pub status: watch::Receiver<Snapshot>,
    stopped: Arc<AtomicBool>,
    transport: Arc<Mutex<Option<Arc<dyn SessionTransport>>>>,
}

impl SessionHandle {
    pub async fn request(&self, request: Request) -> Response {
        let id = request.id;
        if matches!(
            request.operation,
            Operation::Status | Operation::Capabilities | Operation::Doctor
        ) {
            let status = self.status.borrow().clone();
            return match request.operation {
                Operation::Capabilities => Response::ok(id, status.capabilities()),
                Operation::Doctor => Response::ok(
                    id,
                    serde_json::json!({"lifecycle":status.lifecycle,"last_error":status.last_error,"ble":status.ble,"queue_depth":status.queue_depth}),
                ),
                _ => Response::snapshot(id, &status),
            };
        }
        let duration = Duration::from_millis(request.timeout_ms);
        let deadline = Instant::now() + duration;
        let (reply, rx) = oneshot::channel();
        match self.sender.try_send(Envelope {
            request,
            deadline,
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                return Response::failed(
                    id,
                    DaemonError::new(ErrorCode::QueueFull, "queue", "command queue is full"),
                )
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Response::failed(
                    id,
                    DaemonError::new(
                        ErrorCode::ServiceUnavailable,
                        "queue",
                        "session owner stopped",
                    ),
                )
            }
        }
        match tokio::time::timeout(duration, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Response::failed(
                id,
                DaemonError::new(
                    ErrorCode::Cancelled,
                    "command",
                    "session stopped before returning a result",
                ),
            ),
            Err(_) => Response::failed(
                id,
                DaemonError::new(
                    ErrorCode::OutcomeUnknown,
                    "command",
                    "request deadline elapsed; a dispatched change may have applied",
                ),
            ),
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Ok(transport) = self.transport.lock() {
            if let Some(transport) = transport.as_ref() {
                transport.close();
            }
        }
    }
}

pub struct Session {
    pub handle: SessionHandle,
    join: Option<thread::JoinHandle<()>>,
}

impl Session {
    pub fn spawn(backend: Arc<dyn Backend>, config: Config, instance: String) -> Self {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_LIMIT);
        let state = DeviceState::default();
        let mut status = Snapshot::new(instance, &state);
        status.ear_policy = config.ear_policy;
        let (status_tx, status_rx) = watch::channel(status.clone());
        let stopped = Arc::new(AtomicBool::new(false));
        let transport = Arc::new(Mutex::new(None));
        let handle = SessionHandle {
            sender,
            status: status_rx,
            stopped: stopped.clone(),
            transport: transport.clone(),
        };
        let join = thread::spawn(move || {
            Worker {
                backend,
                config,
                state,
                status,
                status_tx,
                receiver,
                stopped,
                transport,
                connection: None,
                active: None,
                queue: VecDeque::new(),
                started: Instant::now(),
                generation: 0,
                retry_at: Instant::now(),
                attempts: 0,
                last_probe: Instant::now(),
                suspended: false,
            }
            .run();
        });
        Self {
            handle,
            join: Some(join),
        }
    }
    pub fn stop(mut self) {
        self.handle.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.handle.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct Worker {
    backend: Arc<dyn Backend>,
    config: Config,
    state: DeviceState,
    status: Snapshot,
    status_tx: watch::Sender<Snapshot>,
    receiver: mpsc::Receiver<Envelope>,
    stopped: Arc<AtomicBool>,
    transport: Arc<Mutex<Option<Arc<dyn SessionTransport>>>>,
    connection: Option<Arc<dyn SessionTransport>>,
    active: Option<Active>,
    queue: VecDeque<Envelope>,
    started: Instant,
    generation: u64,
    retry_at: Instant,
    attempts: u32,
    last_probe: Instant,
    suspended: bool,
}

impl Worker {
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
    fn publish(&mut self) {
        self.status.device = DeviceStatus::from(&self.state);
        self.status.session_generation = self.generation;
        self.status.revision = self.status.revision.saturating_add(1);
        self.status.updated_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.status.observed_monotonic_ms = self.now();
        self.status.queue_depth = self.queue.len() + usize::from(self.active.is_some());
        self.status_tx.send_replace(self.status.clone());
    }
    fn error(&mut self, error: DaemonError) {
        self.status.last_error = Some(error);
        self.publish();
    }
    fn disconnect(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.close();
        }
        if let Ok(mut transport) = self.transport.lock() {
            *transport = None;
        }
        if let Some(mut active) = self.active.take() {
            let action = active.pending.transport_lost();
            let response = self.action_response(active.envelope.request.id, action);
            let _ = active.envelope.reply.send(response);
        }
        self.state.end_session(self.generation);
        self.status.connected = false;
    }
    fn schedule_retry(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
        let exponent = self.attempts.saturating_sub(1).min(4);
        let jitter = (u64::from(self.attempts) * 137 + self.generation * 71) % 500;
        let delay = Duration::from_millis((1000u64 << exponent) + jitter);
        self.retry_at = Instant::now()
            + if self.attempts % 10 == 0 {
                Duration::from_secs(30)
            } else {
                delay
            };
        self.status.retry_count = self.attempts;
        self.status.retry_at_ms = Some(
            self.now()
                + self
                    .retry_at
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64,
        );
        self.status.lifecycle = "recovering".into();
        self.publish();
    }
    fn connect(&mut self) -> Result<(), DaemonError> {
        self.status.lifecycle = "waiting_for_device".into();
        self.publish();
        let endpoint = self.backend.discover(self.config.address.as_deref())?;
        self.generation = self.generation.wrapping_add(1);
        self.state.begin_session(self.generation);
        self.state
            .set_device_identity(&endpoint.address, &endpoint.name);
        self.status.lifecycle = "connecting".into();
        self.publish();
        let connection = self.backend.connect(&endpoint, Duration::from_secs(3))?;
        if let Ok(mut transport) = self.transport.lock() {
            *transport = Some(connection.clone());
        }
        self.connection = Some(connection.clone());
        self.status.lifecycle = "awaiting_handshake".into();
        self.publish();
        let mut handshake = Handshake::new(self.generation, self.now(), 3000);
        let mut action = handshake.start();
        let mut bytes = [0u8; 1024];
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return Err(DaemonError::new(
                    ErrorCode::Cancelled,
                    "handshake",
                    "shutdown requested",
                ));
            }
            match action {
                HandshakeAction::Send(packet) => {
                    connection
                        .send(&packet, Duration::from_secs(1))
                        .map_err(|e| transport_error("handshake send", e))?;
                    action = handshake.mark_subscribed(self.generation, self.now());
                    continue;
                }
                HandshakeAction::Ready => break,
                HandshakeAction::TimedOut => {
                    return Err(DaemonError::transient(
                        ErrorCode::HandshakeTimeout,
                        "handshake",
                        "expected handshake notification was not received",
                    ))
                }
                HandshakeAction::Cancelled => {
                    return Err(DaemonError::new(
                        ErrorCode::Cancelled,
                        "handshake",
                        "session was superseded",
                    ))
                }
                HandshakeAction::Wait => {}
            }
            match connection.recv(&mut bytes, READ_SLICE) {
                Ok(size) => {
                    if let Ok(packet) = parser::parse(&bytes[..size]) {
                        self.state
                            .apply_packet(&packet, self.generation, self.now());
                        action = handshake.on_packet(&packet, self.generation, self.now());
                    } else {
                        self.status.malformed_frames += 1;
                        action = handshake.tick(self.generation, self.now());
                    }
                }
                Err(error) if is_timeout(&error) => {
                    action = handshake.tick(self.generation, self.now())
                }
                Err(error) => return Err(transport_error("handshake receive", error)),
            }
        }
        self.status.lifecycle = "ready".into();
        self.status.connected = true;
        self.status.last_error = None;
        self.status.retry_at_ms = None;
        self.attempts = 0;
        self.last_probe = Instant::now();
        self.publish();
        Ok(())
    }
    fn action_response(&self, id: u64, action: CommandAction) -> Response {
        match action {
            CommandAction::Complete(outcome) => match outcome {
                CommandOutcome::Confirmed | CommandOutcome::AlreadySet => Response::ok(id, outcome),
                _ => Response::failed(
                    id,
                    DaemonError::new(
                        match outcome {
                            CommandOutcome::OutcomeUnknown => ErrorCode::OutcomeUnknown,
                            CommandOutcome::TimedOut => ErrorCode::CommandTimeout,
                            CommandOutcome::Superseded => ErrorCode::Superseded,
                            CommandOutcome::Rejected(_) => ErrorCode::Unsupported,
                            _ => ErrorCode::Cancelled,
                        },
                        "command",
                        format!("{outcome:?}"),
                    ),
                ),
            },
            _ => Response::failed(
                id,
                DaemonError::new(ErrorCode::Internal, "command", "incomplete command action"),
            ),
        }
    }
    fn handle_action(&mut self, mut active: Active, mut action: CommandAction) {
        loop {
            match action {
                CommandAction::Wait => {
                    self.active = Some(active);
                    return;
                }
                CommandAction::Send(packet) => {
                    let remaining = active
                        .envelope
                        .deadline
                        .saturating_duration_since(Instant::now());
                    let result = self
                        .connection
                        .as_ref()
                        .ok_or(TransportError::NotConnected)
                        .and_then(|connection| {
                            connection.send(&packet, remaining.min(Duration::from_secs(1)))
                        });
                    match result {
                        Ok(()) => action = active.pending.mark_sent(self.generation, self.now()),
                        Err(error) => {
                            let _ = active.envelope.reply.send(Response::failed(
                                active.envelope.request.id,
                                DaemonError::new(
                                    ErrorCode::OutcomeUnknown,
                                    "send",
                                    error.to_string(),
                                ),
                            ));
                            self.disconnect();
                            self.schedule_retry();
                            return;
                        }
                    }
                }
                CommandAction::Complete(outcome) => {
                    let uncertain = matches!(outcome, CommandOutcome::OutcomeUnknown);
                    let response = self.action_response(
                        active.envelope.request.id,
                        CommandAction::Complete(outcome),
                    );
                    let _ = active.envelope.reply.send(response);
                    if uncertain {
                        self.disconnect();
                        self.schedule_retry();
                    }
                    self.publish();
                    return;
                }
            }
        }
    }
    fn enqueue(&mut self, envelope: Envelope) {
        if let Operation::Change(change) = &envelope.request.operation {
            if let Some(index)=self.queue.iter().position(|other|matches!(&other.request.operation,Operation::Change(other) if other.id()==change.id())) {
                if let Some(old)=self.queue.remove(index){let _=old.reply.send(Response::failed(old.request.id,DaemonError::new(ErrorCode::Superseded,"queue","replaced by a newer desired value")));}
            }
        }
        if self.queue.len() >= QUEUE_LIMIT {
            let _ = envelope.reply.send(Response::failed(
                envelope.request.id,
                DaemonError::new(ErrorCode::QueueFull, "queue", "command queue full"),
            ));
        } else {
            self.queue.push_back(envelope);
        }
    }
    fn dispatch(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(envelope) = self.queue.pop_front() else {
            return;
        };
        if envelope.reply.is_closed() {
            return;
        }
        if Instant::now() >= envelope.deadline {
            let _ = envelope.reply.send(Response::failed(
                envelope.request.id,
                DaemonError::new(
                    ErrorCode::CommandTimeout,
                    "queue",
                    "request expired before dispatch",
                ),
            ));
            return;
        }
        let id = envelope.request.id;
        let response = match &envelope.request.operation {
            Operation::Status | Operation::Watch => Response::snapshot(id, &self.status),
            Operation::Capabilities => Response::ok(id, self.status.capabilities()),
            Operation::Doctor => Response::ok(
                id,
                serde_json::json!({"lifecycle":self.status.lifecycle,"last_error":self.status.last_error,"ble":self.status.ble,"queue_depth":self.status.queue_depth}),
            ),
            Operation::Change(change) => {
                if !self.status.connected || !self.state.fresh("metadata") {
                    self.queue.push_front(envelope);
                    return;
                }
                let remaining = envelope.deadline.saturating_duration_since(Instant::now());
                let mut pending = PendingChange::new(
                    change.clone(),
                    self.generation,
                    self.now() + remaining.as_millis() as u64,
                );
                let action = pending.begin(&self.state, self.now());
                self.handle_action(Active { envelope, pending }, action);
                return;
            }
            Operation::Reconnect => {
                self.disconnect();
                self.retry_at = Instant::now();
                Response::ok(id, serde_json::json!({"scheduled":true}))
            }
            Operation::EarPolicy(policy) => {
                self.config.ear_policy = *policy;
                self.status.ear_policy = *policy;
                self.publish();
                Response::ok(id, serde_json::json!({"ear_policy":policy}))
            }
            operation => match self.backend.audio(&self.state.bluetooth_address, operation) {
                Ok(value) => {
                    self.status.audio = value.clone();
                    self.publish();
                    Response::ok(id, value)
                }
                Err(error) => Response::failed(id, error),
            },
        };
        let _ = envelope.reply.send(response);
    }
    fn run(&mut self) {
        while !self.stopped.load(Ordering::SeqCst) {
            while let Ok(envelope) = self.receiver.try_recv() {
                self.enqueue(envelope);
            }
            let sleeping = self.backend.sleeping();
            if sleeping && !self.suspended {
                self.suspended = true;
                self.disconnect();
                self.status.lifecycle = "suspended".into();
                self.publish();
            }
            if !sleeping && self.suspended {
                self.suspended = false;
                self.retry_at = Instant::now();
            }
            self.dispatch();
            if !self.suspended && self.connection.is_none() && Instant::now() >= self.retry_at {
                if let Err(error) = self.connect() {
                    self.disconnect();
                    self.error(error);
                    self.schedule_retry();
                }
            }
            if let Some(mut active) = self.active.take() {
                let action = if active.envelope.reply.is_closed() {
                    active.pending.cancel()
                } else {
                    active.pending.tick(self.generation, self.now())
                };
                self.handle_action(active, action);
            }
            if let Some(connection) = self.connection.clone() {
                let mut bytes = [0u8; 1024];
                match connection.recv(&mut bytes, READ_SLICE) {
                    Ok(size) => match parser::parse(&bytes[..size]) {
                        Ok(packet) => {
                            let changed =
                                self.state
                                    .apply_packet(&packet, self.generation, self.now());
                            if let Some(mut active) = self.active.take() {
                                let action =
                                    active
                                        .pending
                                        .on_packet(&packet, self.generation, self.now());
                                self.handle_action(active, action);
                            }
                            if changed {
                                self.publish();
                            }
                        }
                        Err(_) => {
                            self.status.malformed_frames =
                                self.status.malformed_frames.saturating_add(1);
                            if self.status.malformed_frames.is_multiple_of(100) {
                                self.publish();
                            }
                        }
                    },
                    Err(error) if is_timeout(&error) => {}
                    Err(error) => {
                        self.disconnect();
                        self.error(transport_error("receive", error));
                        self.schedule_retry();
                    }
                }
                if self.last_probe.elapsed() > Duration::from_secs(30) {
                    self.last_probe = Instant::now();
                    if let Err(error) = self.backend.discover(Some(&self.state.bluetooth_address)) {
                        self.disconnect();
                        self.error(error);
                        self.schedule_retry();
                    }
                }
            } else {
                thread::park_timeout(READ_SLICE);
            }
        }
        self.status.lifecycle = "stopping".into();
        self.disconnect();
        self.publish();
        while let Some(envelope) = self.queue.pop_front() {
            let _ = envelope.reply.send(Response::failed(
                envelope.request.id,
                DaemonError::new(ErrorCode::Cancelled, "shutdown", "daemon stopping"),
            ));
        }
    }
}
