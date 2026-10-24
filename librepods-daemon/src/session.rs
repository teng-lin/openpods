// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{
    backend::{is_timeout, transport_error, Backend, SessionTransport},
    config::Config,
    ipc::{Operation, Request, Response},
    snapshot::Snapshot,
    telemetry::{KeyRead, Telemetry},
    DaemonError, ErrorCode,
};
use librepods_core::{
    device::state::DeviceState,
    policy::recovery::{
        Lifecycle, RecoveryDecision, RecoveryPolicy, DEFAULT_ABSENT_ATTEMPTS, RECONCILE_INTERVAL_MS,
    },
    protocol::{
        handshake::{Handshake, HandshakeAction, HandshakeStage},
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
const AVAILABILITY_POLL_MS: u64 = 1000;

struct Envelope {
    target_address: Option<String>,
    request: Request,
    deadline: Instant,
    reply: oneshot::Sender<Response>,
    dispatched: Arc<AtomicBool>,
}
struct KeyExchange {
    envelope: Envelope,
    keys: KeyRead,
}
struct Active {
    envelope: Envelope,
    pending: PendingChange,
}

#[derive(Clone)]
pub struct SessionHandle {
    sender: mpsc::SyncSender<Envelope>,
    backend: Arc<dyn Backend>,
    pub(crate) audio_feed: watch::Sender<serde_json::Value>,
    pub(crate) publication_health: watch::Sender<Option<DaemonError>>,
    pub(crate) manual_overrides: watch::Sender<u64>,
    pub status: watch::Receiver<Snapshot>,
    stopped: Arc<AtomicBool>,
    transport: Arc<Mutex<Option<Arc<dyn SessionTransport>>>>,
}

impl SessionHandle {
    pub async fn request(&self, mut request: Request) -> Response {
        let id = request.id;
        let target_address = if let Operation::ForDevice { address, operation } = request.operation
        {
            request.operation = *operation;
            if matches!(request.operation, Operation::ForDevice { .. }) {
                return Response::failed(
                    id,
                    DaemonError::new(
                        ErrorCode::InvalidInput,
                        "device selection",
                        "nested device assertions are not supported",
                    ),
                );
            }
            Some(address)
        } else {
            None
        };
        if target_address.as_ref().is_some_and(|address| {
            !address.eq_ignore_ascii_case(&self.status.borrow().device.address)
        }) {
            return Response::failed(
                id,
                DaemonError::new(
                    ErrorCode::DeviceUnavailable,
                    "device selection",
                    "service owns a different device; start it with the requested address",
                ),
            );
        }
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
        let readonly = matches!(request.operation, Operation::FetchKeys);
        let duration = Duration::from_millis(request.timeout_ms);
        let readonly_audio = matches!(request.operation, Operation::AudioStatus);
        if matches!(
            request.operation,
            Operation::AudioStatus
                | Operation::SetOutput(_)
                | Operation::SetInput(_)
                | Operation::SetProfile(_)
                | Operation::MoveStream { .. }
                | Operation::Unmute
        ) {
            let (audio_generation, address) = {
                let status = self.status.borrow();
                (status.session_generation, status.device.address.clone())
            };
            let audio_address = address.clone();
            if !matches!(request.operation, Operation::AudioStatus) {
                self.manual_overrides
                    .send_modify(|revision| *revision = revision.saturating_add(1));
            }
            let result = tokio::time::timeout(
                duration,
                self.backend.audio_async(address, request.operation),
            )
            .await;
            let changed_session = {
                let status = self.status.borrow();
                status.session_generation != audio_generation
                    || status.device.address != audio_address
            };
            if matches!(&result, Ok(Ok(_)))
                && (changed_session || self.stopped.load(Ordering::SeqCst))
            {
                return Response::failed(id,DaemonError::new(if readonly_audio {ErrorCode::Cancelled} else {ErrorCode::OutcomeUnknown},"audio","session changed while audio operation was in flight; old result was not published"));
            }
            return match result {
                Ok(Ok(value))=>{self.audio_feed.send_replace(value.clone());Response::ok(id,value)},
                Ok(Err(error))=>Response::failed(id,error),
                Err(_)=>Response::failed(id,DaemonError::new(if readonly_audio{ErrorCode::CommandTimeout}else{ErrorCode::OutcomeUnknown},"audio","audio operation deadline elapsed; inspect status before retrying a mutation")),
            };
        }
        let deadline = Instant::now() + duration;
        let dispatched = Arc::new(AtomicBool::new(false));
        let (reply, rx) = oneshot::channel();
        match self.sender.try_send(Envelope {
            target_address,
            request,
            deadline,
            reply,
            dispatched: dispatched.clone(),
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
                    if !readonly && dispatched.load(Ordering::SeqCst) {
                        ErrorCode::OutcomeUnknown
                    } else {
                        ErrorCode::CommandTimeout
                    },
                    "command",
                    "request deadline elapsed; inspect status before retrying a dispatched change",
                ),
            ),
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.backend.cancel();
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
        Self::spawn_inner(backend, config, instance, None)
    }
    pub fn spawn_persistent(
        backend: Arc<dyn Backend>,
        config: Config,
        instance: String,
        paths: crate::storage::Paths,
    ) -> Self {
        Self::spawn_inner(backend, config, instance, Some(paths))
    }
    fn spawn_inner(
        backend: Arc<dyn Backend>,
        config: Config,
        instance: String,
        paths: Option<crate::storage::Paths>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_LIMIT);
        let mut state = DeviceState::default();
        if let Some(address) = &config.address {
            state.set_device_identity(address, "");
        }
        let mut status = Snapshot::new(instance, &state);
        status.ear_policy = config.ear_policy;
        let (status_tx, status_rx) = watch::channel(status.clone());
        let (audio_feed, audio_updates) = watch::channel(status.audio.clone());
        let (publication_health, publication_updates) = watch::channel(None);
        let (manual_overrides, _) = watch::channel(0);
        let stopped = Arc::new(AtomicBool::new(false));
        let transport = Arc::new(Mutex::new(None));
        let handle = SessionHandle {
            sender,
            backend: backend.clone(),
            audio_feed,
            publication_health,
            manual_overrides,
            status: status_rx,
            stopped: stopped.clone(),
            transport: transport.clone(),
        };
        let join = thread::spawn(move || {
            let started = Instant::now();
            let telemetry = Telemetry::new(started, paths.clone());
            Worker {
                backend,
                config,
                paths,
                state,
                status,
                status_tx,
                audio_updates,
                publication_updates,
                receiver,
                stopped,
                transport,
                connection: None,
                active: None,
                key_exchange: None,
                telemetry,
                queue: VecDeque::new(),
                started,
                generation: 0,
                recovery: {
                    let mut policy = RecoveryPolicy::new(DEFAULT_ABSENT_ATTEMPTS);
                    policy.reconnect_explicitly(0);
                    policy
                },
                next_probe_ms: 0,
                adapter_ready: false,
                bluez_connected: false,
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
    paths: Option<crate::storage::Paths>,
    state: DeviceState,
    status: Snapshot,
    status_tx: watch::Sender<Snapshot>,
    audio_updates: watch::Receiver<serde_json::Value>,
    publication_updates: watch::Receiver<Option<DaemonError>>,
    receiver: mpsc::Receiver<Envelope>,
    stopped: Arc<AtomicBool>,
    transport: Arc<Mutex<Option<Arc<dyn SessionTransport>>>>,
    connection: Option<Arc<dyn SessionTransport>>,
    active: Option<Active>,
    key_exchange: Option<KeyExchange>,
    telemetry: Telemetry,
    queue: VecDeque<Envelope>,
    started: Instant,
    generation: u64,
    recovery: RecoveryPolicy,
    next_probe_ms: u64,
    adapter_ready: bool,
    bluez_connected: bool,
}

impl Worker {
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
    fn publish(&mut self) {
        self.status.session_generation = self.generation;
        self.status.revision = self.status.revision.saturating_add(1);
        self.status.updated_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.status.observed_monotonic_ms = self.now();
        self.status.observe_device(
            &self.state,
            self.status.observed_monotonic_ms,
            self.status.updated_unix_ms,
        );
        self.status.queue_depth = self.queue.len()
            + usize::from(self.active.is_some())
            + usize::from(self.key_exchange.is_some());
        self.status_tx.send_replace(self.status.clone());
    }
    fn error(&mut self, error: DaemonError) {
        self.status.last_error = Some(error);
        self.publish();
    }
    fn disconnect(&mut self) {
        if let Some(exchange) = self.key_exchange.take() {
            let _ = exchange.envelope.reply.send(Response::failed(
                exchange.envelope.request.id,
                DaemonError::new(
                    ErrorCode::Cancelled,
                    "BLE keys",
                    "key acquisition interrupted; no keys persisted",
                ),
            ));
        }
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
    fn sync_recovery(&mut self) {
        self.status.lifecycle = match self.recovery.state {
            Lifecycle::WaitingForAdapter => "waiting_for_adapter",
            Lifecycle::WaitingForDevice => "waiting_for_device",
            Lifecycle::Connecting => "connecting",
            Lifecycle::AwaitingHandshake => "awaiting_handshake",
            Lifecycle::AwaitingFeatures => "awaiting_features",
            Lifecycle::Subscribing => "subscribing",
            Lifecycle::Ready => "ready",
            Lifecycle::Recovering => "recovering",
            Lifecycle::Suspended => "suspended",
            Lifecycle::Stopping => "stopping",
        }
        .into();
        self.status.retry_count = self
            .recovery
            .connected_attempts
            .saturating_add(self.recovery.absent_attempts);
        self.status.retry_at_ms = self.recovery.due_ms;
    }
    fn lifecycle(&mut self, state: Lifecycle) {
        self.recovery.state = state;
        self.sync_recovery();
        self.publish();
    }
    fn schedule_retry(&mut self) {
        if matches!(
            self.recovery.state,
            Lifecycle::Suspended | Lifecycle::Stopping
        ) {
            return;
        }
        if !self.adapter_ready {
            self.recovery.state = Lifecycle::WaitingForAdapter;
            self.recovery.due_ms = None;
        } else if self.status.last_error.as_ref().is_some_and(|error| {
            matches!(
                error.code,
                ErrorCode::PermissionDenied
                    | ErrorCode::Unsupported
                    | ErrorCode::InvalidInput
                    | ErrorCode::VersionMismatch
            )
        }) {
            // A configuration/permission failure needs an explicit reconnect after correction.
            self.recovery.disconnect_intentionally();
        } else {
            let attempt = u64::from(self.recovery.connected_attempts)
                + u64::from(self.recovery.absent_attempts)
                + 1;
            let jitter = attempt
                .wrapping_mul(137)
                .wrapping_add(self.generation.wrapping_mul(71))
                % 500;
            self.recovery
                .on_failure(self.now(), self.bluez_connected, jitter);
        }
        self.sync_recovery();
        self.next_probe_ms = self.now();
        self.publish();
    }
    fn probe_availability(&mut self) {
        let now = self.now();
        if now < self.next_probe_ms {
            return;
        }
        self.next_probe_ms = now.saturating_add(if self.connection.is_some() {
            RECONCILE_INTERVAL_MS
        } else {
            AVAILABILITY_POLL_MS
        });
        let address = self.config.address.as_deref().or_else(|| {
            (!self.state.bluetooth_address.is_empty())
                .then_some(self.state.bluetooth_address.as_str())
        });
        let prior = (self.adapter_ready, self.bluez_connected);
        let discovered = self.backend.discover(address);
        self.adapter_ready = discovered
            .as_ref()
            .err()
            .is_none_or(|error| error.code != ErrorCode::AdapterUnavailable);
        self.bluez_connected = discovered.is_ok();
        if let Err(error) = discovered {
            let changed = prior != (self.adapter_ready, self.bluez_connected)
                || self.status.last_error.as_ref() != Some(&error);
            if self.connection.is_some() {
                self.disconnect();
                self.status.last_error = Some(error);
                self.schedule_retry();
                return;
            }
            if !self.adapter_ready {
                self.recovery.state = Lifecycle::WaitingForAdapter;
                self.recovery.due_ms = None;
            }
            if changed {
                self.status.last_error = Some(error);
                self.sync_recovery();
                self.publish();
            }
        }
        if self.recovery.is_desired()
            && self.connection.is_none()
            && ((!prior.0 && self.adapter_ready) || (!prior.1 && self.bluez_connected))
        {
            self.recovery.reconnect_explicitly(now);
            self.sync_recovery();
            self.publish();
        }
    }
    fn connect(&mut self) -> Result<(), DaemonError> {
        self.lifecycle(Lifecycle::WaitingForDevice);
        let address = self.config.address.as_deref().or_else(|| {
            (!self.state.bluetooth_address.is_empty())
                .then_some(self.state.bluetooth_address.as_str())
        });
        let endpoint = match self.backend.discover(address) {
            Ok(endpoint) => {
                self.adapter_ready = true;
                self.bluez_connected = true;
                endpoint
            }
            Err(error) => {
                self.adapter_ready = error.code != ErrorCode::AdapterUnavailable;
                self.bluez_connected = false;
                return Err(error);
            }
        };
        self.generation = self
            .generation
            .max(self.recovery.generation)
            .saturating_add(1);
        self.recovery.generation = self.generation;
        self.state
            .set_device_identity(&endpoint.address, &endpoint.name);
        self.state.begin_session(self.generation);
        self.recovery.state = Lifecycle::Connecting;
        self.sync_recovery();
        self.poll_telemetry(false);
        self.telemetry.pause_for_connection()?;
        self.publish();
        let connection = self.backend.connect(&endpoint, Duration::from_secs(3))?;
        if let Ok(mut transport) = self.transport.lock() {
            *transport = Some(connection.clone());
        }
        self.connection = Some(connection.clone());
        self.lifecycle(Lifecycle::AwaitingHandshake);
        let mut handshake = Handshake::new(self.generation, self.now(), 3000);
        let mut action = handshake.start();
        let mut bytes = [0u8; 1024];
        loop {
            let stage = match handshake.stage {
                HandshakeStage::Features => Lifecycle::AwaitingFeatures,
                HandshakeStage::Subscription | HandshakeStage::Ready => Lifecycle::Subscribing,
                _ => Lifecycle::AwaitingHandshake,
            };
            if self.recovery.state != stage {
                self.lifecycle(stage);
            }
            if self.backend.sleeping() {
                self.recovery.suspend();
                self.sync_recovery();
                return Err(DaemonError::new(
                    ErrorCode::Cancelled,
                    "handshake",
                    "suspend interrupted handshake",
                ));
            }
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
        self.recovery.ready(self.now());
        self.sync_recovery();
        self.status.connected = true;
        self.status.last_error = None;
        self.next_probe_ms = self.now().saturating_add(RECONCILE_INTERVAL_MS);
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
                    self.publish();
                    return;
                }
                CommandAction::Send(packet) => {
                    // The pure action may have been produced before cancellation, shutdown
                    // or scheduler delay. Revalidate at the actual write boundary.
                    if self.stopped.load(Ordering::SeqCst) || active.envelope.reply.is_closed() {
                        action = active.pending.cancel();
                        continue;
                    }
                    let remaining = active
                        .envelope
                        .deadline
                        .saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        action = active
                            .pending
                            .tick(self.generation, active.pending.deadline_ms.max(self.now()));
                        continue;
                    }
                    let Some(connection) = self.connection.clone() else {
                        action = active.pending.transport_lost();
                        continue;
                    };
                    active.envelope.dispatched.store(true, Ordering::SeqCst);
                    let result = connection.send(&packet, remaining.min(Duration::from_secs(1)));
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
        if self.active.is_some() || self.key_exchange.is_some() {
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
        if envelope
            .target_address
            .as_ref()
            .is_some_and(|address| !address.eq_ignore_ascii_case(&self.state.bluetooth_address))
        {
            let _ = envelope.reply.send(Response::failed(
                id,
                DaemonError::new(
                    ErrorCode::DeviceUnavailable,
                    "device selection",
                    "selected device changed before dispatch",
                ),
            ));
            return;
        }
        let response = match &envelope.request.operation {
            Operation::Status | Operation::Watch => Response::snapshot(id, &self.status),
            Operation::Capabilities => Response::ok(id, self.status.capabilities()),
            Operation::Doctor => Response::ok(
                id,
                serde_json::json!({"lifecycle":self.status.lifecycle,"last_error":self.status.last_error,"ble":self.status.ble,"queue_depth":self.status.queue_depth}),
            ),
            Operation::Change(change) => {
                if !self.status.connected {
                    let _ = envelope.reply.send(Response::failed(
                        id,
                        DaemonError::new(
                            ErrorCode::DeviceUnavailable,
                            "command",
                            "AirPods control connection is not ready",
                        ),
                    ));
                    return;
                }
                if !self.state.fresh("metadata") {
                    self.queue.push_back(envelope);
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
            Operation::FetchKeys => {
                if !cfg!(feature = "ble") {
                    Response::failed(
                        id,
                        DaemonError::new(
                            ErrorCode::Unsupported,
                            "BLE keys",
                            "BLE support was not compiled",
                        ),
                    )
                } else if self.paths.is_none() {
                    Response::failed(
                        id,
                        DaemonError::new(
                            ErrorCode::Unsupported,
                            "BLE keys",
                            "key persistence requires the persistent service",
                        ),
                    )
                } else if !self.status.connected || !self.state.fresh("metadata") {
                    Response::failed(
                        id,
                        DaemonError::new(
                            ErrorCode::DeviceUnavailable,
                            "BLE keys",
                            "AirPods control connection and identity must be ready",
                        ),
                    )
                } else {
                    let remaining = envelope.deadline.saturating_duration_since(Instant::now());
                    if envelope.reply.is_closed() || self.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    if remaining.is_zero() {
                        let _ = envelope.reply.send(Response::failed(
                            id,
                            DaemonError::new(
                                ErrorCode::CommandTimeout,
                                "BLE keys",
                                "request expired before dispatch",
                            ),
                        ));
                        return;
                    }
                    envelope.dispatched.store(true, Ordering::SeqCst);
                    let result = self
                        .connection
                        .as_ref()
                        .ok_or(TransportError::NotConnected)
                        .and_then(|connection| {
                            connection.send(
                                &librepods_core::protocol::packets::REQUEST_MAGIC_CLOUD_KEYS,
                                remaining.min(Duration::from_secs(1)),
                            )
                        });
                    match result {
                        Ok(()) => {
                            self.key_exchange = Some(KeyExchange {
                                envelope,
                                keys: KeyRead::default(),
                            });
                            self.publish();
                            return;
                        }
                        Err(error) => {
                            self.disconnect();
                            self.schedule_retry();
                            Response::failed(id, transport_error("BLE key request", error))
                        }
                    }
                }
            }
            Operation::ForgetKeys => match self.telemetry.forget(&self.state.bluetooth_address) {
                Ok(()) => Response::ok(id, serde_json::json!({"keys_available":false})),
                Err(error) => Response::failed(id, error),
            },
            Operation::BleEnabled(enabled) => {
                let mut config = self.config.clone();
                config.ble_enabled = *enabled;
                let result = config.validate().and_then(|()| match &self.paths {
                    Some(paths) => paths.save_config(&config),
                    None => Ok(()),
                });
                match result {
                    Ok(()) => {
                        self.config = config;
                        self.poll_telemetry(false);
                        self.publish();
                        Response::ok(id, self.status.ble.clone())
                    }
                    Err(error) => Response::failed(id, error),
                }
            }
            Operation::Reconnect => {
                envelope.dispatched.store(true, Ordering::SeqCst);
                self.disconnect();
                self.recovery.reconnect_explicitly(self.now());
                self.next_probe_ms = 0;
                self.sync_recovery();
                Response::ok(id, serde_json::json!({"scheduled":true}))
            }
            Operation::EarPolicy(policy) => {
                envelope.dispatched.store(true, Ordering::SeqCst);
                let mut config = self.config.clone();
                config.ear_policy = *policy;
                if let Some(paths) = &self.paths {
                    if let Err(error) = paths.save_config(&config) {
                        let _ = envelope.reply.send(Response::failed(id, error));
                        return;
                    }
                }
                self.config = config;
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
    fn poll_telemetry(&mut self, allowed: bool) {
        let before = self.state.observations.clone();
        let ble = self.telemetry.poll(
            &mut self.state,
            self.generation,
            self.config.ble_enabled,
            allowed,
        );
        if self.status.ble != ble || before != self.state.observations {
            self.status.ble_enabled = self.config.ble_enabled;
            self.status.ble = ble;
            self.publish();
        }
    }
    fn expire_key_exchange(&mut self) {
        if self.key_exchange.as_ref().is_some_and(|exchange| {
            exchange.envelope.reply.is_closed() || Instant::now() >= exchange.envelope.deadline
        }) {
            if let Some(exchange) = self.key_exchange.take() {
                let _ = exchange.envelope.reply.send(Response::failed(
                    exchange.envelope.request.id,
                    DaemonError::new(
                        ErrorCode::CommandTimeout,
                        "BLE keys",
                        "key request expired; no keys persisted",
                    ),
                ));
            }
            // A late old key response must not satisfy a subsequent exchange.
            self.disconnect();
            self.schedule_retry();
        }
    }
    fn run(&mut self) {
        while !self.stopped.load(Ordering::SeqCst) {
            if self.publication_updates.has_changed().unwrap_or(false) {
                self.status.publication_error =
                    self.publication_updates.borrow_and_update().clone();
                self.publish();
            }
            if self.audio_updates.has_changed().unwrap_or(false) {
                self.status.audio =
                    crate::snapshot::bounded_audio(self.audio_updates.borrow_and_update().clone());
                self.publish();
            }
            while let Ok(envelope) = self.receiver.try_recv() {
                self.enqueue(envelope);
            }
            let sleeping = self.backend.sleeping();
            if sleeping && self.recovery.state != Lifecycle::Suspended {
                self.disconnect();
                self.recovery.suspend();
                self.sync_recovery();
                self.publish();
            }
            if !sleeping && self.recovery.state == Lifecycle::Suspended {
                self.recovery.resume(self.now());
                self.next_probe_ms = 0;
                self.sync_recovery();
                self.publish();
            }
            self.expire_key_exchange();
            self.dispatch();
            self.poll_telemetry(
                !sleeping
                    && self.recovery.state != Lifecycle::Suspended
                    && self.connection.is_none()
                    && self.status.lifecycle == "waiting_for_device",
            );
            if !sleeping && self.recovery.state != Lifecycle::Suspended {
                self.probe_availability();
                let have_address =
                    self.config.address.is_some() || !self.state.bluetooth_address.is_empty();
                if self.connection.is_none()
                    && self.recovery.tick(
                        self.now(),
                        self.adapter_ready,
                        self.bluez_connected,
                        have_address,
                    ) == RecoveryDecision::Reconcile
                {
                    if let Err(error) = self.connect() {
                        self.disconnect();
                        self.error(error);
                        self.schedule_retry();
                    }
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
                            if let Some(mut exchange) = self.key_exchange.take() {
                                if !exchange.envelope.reply.is_closed()
                                    && Instant::now() < exchange.envelope.deadline
                                    && exchange.keys.observe(&packet)
                                {
                                    let result = self
                                        .telemetry
                                        .persist(&self.state.bluetooth_address, &exchange.keys);
                                    let response = match result {
                                        Ok(()) => Response::ok(
                                            exchange.envelope.request.id,
                                            serde_json::json!({"keys_available":true}),
                                        ),
                                        Err(error) => {
                                            Response::failed(exchange.envelope.request.id, error)
                                        }
                                    };
                                    let _ = exchange.envelope.reply.send(response);
                                } else {
                                    self.key_exchange = Some(exchange);
                                }
                            }
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
            } else {
                thread::park_timeout(READ_SLICE);
            }
        }
        self.recovery.stop();
        self.sync_recovery();
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

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use librepods_core::{
        device::state::NoiseControlMode,
        protocol::{parser::ParsedPacket, settings::SettingChange},
    };
    use std::sync::atomic::AtomicUsize;
    struct NoDiscovery;
    impl Backend for NoDiscovery {
        fn discover(&self, _: Option<&str>) -> Result<crate::backend::Endpoint, DaemonError> {
            panic!("dispatch guard must not discover")
        }
        fn connect(
            &self,
            _: &crate::backend::Endpoint,
            _: Duration,
        ) -> Result<Arc<dyn SessionTransport>, DaemonError> {
            panic!("dispatch guard must not reconnect")
        }
    }
    struct Spy(AtomicUsize);
    impl SessionTransport for Spy {
        fn send(&self, _: &[u8], _: Duration) -> Result<(), TransportError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn recv(&self, _: &mut [u8], _: Duration) -> Result<usize, TransportError> {
            panic!("dispatch guard must not read")
        }
        fn close(&self) {}
    }
    fn owner(transport: Arc<Spy>) -> Worker {
        let started = Instant::now();
        let mut state = DeviceState::default();
        state.begin_session(1);
        state.apply_packet(
            &ParsedPacket::Metadata {
                name: "Example".into(),
                model_number: "A3048".into(),
                manufacturer: "Apple Inc.".into(),
            },
            1,
            0,
        );
        state.apply_packet(&ParsedPacket::NoiseControl(NoiseControlMode::Off), 1, 0);
        let status = Snapshot::new("dispatch-test".into(), &state);
        let (status_tx, _) = watch::channel(status.clone());
        let (_, audio_updates) = watch::channel(status.audio.clone());
        let (_, publication_updates) = watch::channel(None);
        let (_, receiver) = mpsc::sync_channel(QUEUE_LIMIT);
        Worker {
            backend: Arc::new(NoDiscovery),
            config: Config::default(),
            paths: None,
            state,
            status,
            status_tx,
            audio_updates,
            publication_updates,
            receiver,
            stopped: Arc::new(AtomicBool::new(false)),
            transport: Arc::new(Mutex::new(Some(transport.clone()))),
            connection: Some(transport),
            active: None,
            key_exchange: None,
            telemetry: Telemetry::new(started, None),
            queue: VecDeque::new(),
            started,
            generation: 1,
            recovery: RecoveryPolicy::new(DEFAULT_ABSENT_ATTEMPTS),
            next_probe_ms: 0,
            adapter_ready: true,
            bluez_connected: true,
        }
    }
    #[test]
    fn expired_cancelled_or_stopping_requests_cannot_write_after_a_pure_send_action() {
        for reason in 0..4 {
            let transport = Arc::new(Spy(AtomicUsize::new(0)));
            let mut owner = owner(transport.clone());
            if reason == 3 {
                owner.connection = None;
            }
            let change = SettingChange::ListeningMode(NoiseControlMode::NoiseCancellation);
            let mut pending = PendingChange::new(change.clone(), 1, 10_000);
            let action = pending.begin(&owner.state, 0);
            assert!(matches!(action, CommandAction::Send(_)));
            let (reply, rx) = oneshot::channel();
            let mut rx = Some(rx);
            if reason == 1 {
                drop(rx.take());
            }
            if reason == 2 {
                owner.stopped.store(true, Ordering::SeqCst);
            }
            let dispatched = Arc::new(AtomicBool::new(false));
            let envelope = Envelope {
                target_address: None,
                request: Request {
                    version: 1,
                    id: 1,
                    timeout_ms: 1000,
                    operation: Operation::Change(change),
                },
                deadline: if reason == 0 {
                    Instant::now() - Duration::from_millis(1)
                } else {
                    Instant::now() + Duration::from_secs(1)
                },
                reply,
                dispatched: dispatched.clone(),
            };
            owner.handle_action(Active { envelope, pending }, action);
            assert_eq!(
                transport.0.load(Ordering::SeqCst),
                0,
                "request reason {reason} wrote after becoming ineligible"
            );
            assert!(!dispatched.load(Ordering::SeqCst));
            if let Some(mut rx) = rx {
                assert_eq!(
                    rx.try_recv().unwrap().error.unwrap().code,
                    if reason == 0 {
                        ErrorCode::CommandTimeout
                    } else {
                        ErrorCode::Cancelled
                    }
                );
            }
        }
    }
}
