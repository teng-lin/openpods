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
        settings::{CommandAction, CommandOutcome, CommandRejection, PendingChange},
    },
    transport::TransportError,
};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
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
    suspend_epoch: u64,
    peer: Option<Arc<OwnedFd>>,
    target_address: Option<String>,
    request: Request,
    deadline: Instant,
    reply: oneshot::Sender<Response>,
    dispatched: Arc<AtomicBool>,
}
/// Kernel liveness closes the scheduling gap before Tokio observes peer EOF.
/// A read half-close cancels requests, matching the native pending-reader contract.
fn peer_connected(peer: &OwnedFd) -> bool {
    let closed = libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    let mut descriptor = libc::pollfd {
        fd: peer.as_raw_fd(),
        events: libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: descriptor is valid for this call and its OwnedFd cannot be reused.
    // A zero timeout never waits or consumes any unread/pipelined bytes.
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    result >= 0 && descriptor.revents & closed == 0
}
impl Envelope {
    fn cancel_before_dispatch(self) {
        let _ = self.reply.send(Response::failed(
            self.request.id,
            DaemonError::new(
                ErrorCode::Cancelled,
                "IPC peer",
                "request peer is no longer available before dispatch",
            ),
        ));
    }
    fn client_cancelled(&self) -> bool {
        self.reply.is_closed() || self.peer.as_ref().is_some_and(|peer| !peer_connected(peer))
    }
}
struct KeyExchange {
    envelope: Envelope,
    keys: KeyRead,
}
struct ReadbackConnection {
    address: String,
    generation: u64,
    deadline: Instant,
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
    pub async fn request(&self, request: Request) -> Response {
        self.request_inner(request, None).await
    }
    /// Attach the owned server-side native peer descriptor for dispatch-time liveness.
    pub async fn request_from_peer(&self, request: Request, peer: Arc<OwnedFd>) -> Response {
        self.request_inner(request, Some(peer)).await
    }
    async fn request_inner(&self, mut request: Request, peer: Option<Arc<OwnedFd>>) -> Response {
        let id = request.id;
        let admission_suspend_epoch = self.backend.suspend_epoch();
        if peer.as_ref().is_some_and(|peer| !peer_connected(peer)) {
            return Response::failed(
                id,
                DaemonError::new(
                    ErrorCode::Cancelled,
                    "IPC peer",
                    "request peer closed before dispatch",
                ),
            );
        }
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
                Operation::Doctor => Response::ok(id, status.doctor()),
                _ => Response::snapshot(id, &status),
            };
        }
        if self.backend.sleeping()
            || self.status.borrow().lifecycle == "suspended"
            || self.backend.suspend_epoch() != admission_suspend_epoch
        {
            return Response::failed(
                id,
                DaemonError::new(
                    ErrorCode::Cancelled,
                    "suspend",
                    "request cannot cross a pending suspend cycle",
                ),
            );
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
            let audio_suspend_epoch = admission_suspend_epoch;
            if peer.as_ref().is_some_and(|peer| !peer_connected(peer)) {
                return Response::failed(
                    id,
                    DaemonError::new(
                        ErrorCode::Cancelled,
                        "IPC peer",
                        "request peer closed before audio dispatch",
                    ),
                );
            }
            if self.backend.sleeping()
                || self.backend.suspend_epoch() != admission_suspend_epoch
                || self.status.borrow().lifecycle == "suspended"
            {
                return Response::failed(
                    id,
                    DaemonError::new(
                        ErrorCode::Cancelled,
                        "suspend",
                        "audio request crossed suspend before dispatch",
                    ),
                );
            }
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
                    || status.lifecycle == "suspended"
                    || self.backend.suspend_epoch() != audio_suspend_epoch
                    || self.backend.sleeping()
            };
            if matches!(&result, Ok(Ok(_)))
                && (changed_session || self.stopped.load(Ordering::SeqCst))
            {
                return Response::failed(id,DaemonError::new(if readonly_audio {ErrorCode::Cancelled} else {ErrorCode::OutcomeUnknown},"audio","session or suspend epoch changed while audio operation was in flight; old result was not published"));
            }
            return match result {
                Ok(Ok(value))=>{self.audio_feed.send_replace(value.clone());Response::ok(id,value)},
                Ok(Err(mut error))=>{
                    if !readonly_audio && error.code == ErrorCode::CommandTimeout {error.code=ErrorCode::OutcomeUnknown;error.retryable=false;}
                    Response::failed(id,error)
                },
                Err(_)=>Response::failed(id,DaemonError::new(if readonly_audio{ErrorCode::CommandTimeout}else{ErrorCode::OutcomeUnknown},"audio","audio operation deadline elapsed; inspect status before retrying a mutation")),
            };
        }
        let deadline = Instant::now() + duration;
        let dispatched = Arc::new(AtomicBool::new(false));
        let (reply, rx) = oneshot::channel();
        match self.sender.try_send(Envelope {
            suspend_epoch: admission_suspend_epoch,
            peer,
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
                handled_suspend_epoch: 0,
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
    handled_suspend_epoch: u64,
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
        if let Some(mut active) = self.active.take() {
            let action = active.pending.transport_lost();
            let response = self.action_response(active.envelope.request.id, action);
            let _ = active.envelope.reply.send(response);
        }
        self.release_transport();
    }
    /// Release only the owned vendor channel; a readback transaction stays active.
    fn release_transport(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.close();
        }
        if let Ok(mut transport) = self.transport.lock() {
            *transport = None;
        }
        self.state.end_session(self.generation);
        self.status.connected = false;
    }
    fn handle_suspend(&mut self) {
        let epoch = self.backend.suspend_epoch();
        let changed =
            self.recovery.state != Lifecycle::Suspended || epoch != self.handled_suspend_epoch;
        if changed {
            self.disconnect();
            self.recovery.suspend();
            self.handled_suspend_epoch = epoch;
            self.sync_recovery();
        }
        let had_queued = !self.queue.is_empty();
        while let Some(envelope) = self.queue.pop_front() {
            let _ = envelope.reply.send(Response::failed(
                envelope.request.id,
                DaemonError::new(
                    ErrorCode::Cancelled,
                    "suspend",
                    "queued request cancelled before resume",
                ),
            ));
        }
        if changed || had_queued {
            self.publish();
        }
        // Budget checks cannot clear this evidence. A newer edge remains pending.
        self.backend.acknowledge_suspend(epoch);
    }

    fn sync_recovery(&mut self) {
        self.status.adapter_ready = self.adapter_ready;
        self.status.bluez_connected = self.bluez_connected;
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
    fn connection_budget(
        &self,
        readback: Option<&ReadbackConnection>,
        limit: Duration,
    ) -> Result<Duration, DaemonError> {
        if self.stopped.load(Ordering::SeqCst) || self.backend.sleeping() {
            return Err(DaemonError::new(
                ErrorCode::Cancelled,
                "connection",
                "connection preparation interrupted",
            ));
        }
        if let Some(readback) = readback {
            if self.active.as_ref().is_none_or(|active| {
                active.envelope.client_cancelled()
                    || active.envelope.suspend_epoch != self.backend.suspend_epoch()
            }) {
                return Err(DaemonError::new(
                    ErrorCode::Cancelled,
                    "readback",
                    "request was cancelled",
                ));
            }
            let remaining = readback.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DaemonError::new(
                    ErrorCode::CommandTimeout,
                    "readback",
                    "original command deadline elapsed",
                ));
            }
            Ok(limit.min(remaining))
        } else {
            Ok(limit)
        }
    }
    fn connect(&mut self) -> Result<(), DaemonError> {
        self.connect_bounded(None)
    }
    fn connect_bounded(
        &mut self,
        readback: Option<&ReadbackConnection>,
    ) -> Result<(), DaemonError> {
        self.connection_budget(readback, Duration::from_secs(3))?;
        self.lifecycle(Lifecycle::WaitingForDevice);
        let address = self.config.address.as_deref().or_else(|| {
            (!self.state.bluetooth_address.is_empty())
                .then_some(self.state.bluetooth_address.as_str())
        });
        let address = readback
            .map(|readback| readback.address.as_str())
            .or(address);
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
        self.connection_budget(readback, Duration::from_secs(3))?;
        let generation = self
            .generation
            .max(self.recovery.generation)
            .saturating_add(1);
        if readback.is_some_and(|readback| {
            !readback.address.eq_ignore_ascii_case(&endpoint.address)
                || generation != readback.generation
        }) {
            return Err(DaemonError::new(
                ErrorCode::Cancelled,
                "readback",
                "device identity or expected generation changed",
            ));
        }
        self.generation = generation;
        self.recovery.generation = self.generation;
        self.state
            .set_device_identity(&endpoint.address, &endpoint.name);
        self.state.begin_session(self.generation);
        self.recovery.state = Lifecycle::Connecting;
        self.sync_recovery();
        self.poll_telemetry(false);
        if readback.is_some() {
            self.telemetry.pause_for_connection_with_timeout(
                self.connection_budget(readback, Duration::from_millis(500))?,
            )?;
        } else {
            self.telemetry.pause_for_connection()?;
        }
        self.publish();
        let timeout = self.connection_budget(readback, Duration::from_secs(3))?;
        if readback.is_some() {
            self.status.readback_attempts_total =
                self.status.readback_attempts_total.saturating_add(1);
        } else if self.generation > 1 {
            self.status.reconnect_attempts_total =
                self.status.reconnect_attempts_total.saturating_add(1);
        }
        let connection = self.backend.connect(&endpoint, timeout)?;
        if let Err(error) = self.connection_budget(readback, Duration::from_secs(1)) {
            connection.close();
            return Err(error);
        }
        if let Ok(mut transport) = self.transport.lock() {
            *transport = Some(connection.clone());
        }
        self.connection = Some(connection.clone());
        self.lifecycle(Lifecycle::AwaitingHandshake);
        let budget = self
            .connection_budget(readback, Duration::from_secs(3))?
            .as_millis() as u64;
        let mut handshake = Handshake::new(self.generation, self.now(), budget);
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
                        .send(
                            &packet,
                            self.connection_budget(readback, Duration::from_secs(1))?,
                        )
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
            match connection.recv(&mut bytes, self.connection_budget(readback, READ_SLICE)?) {
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
        self.connection_budget(readback, Duration::from_secs(1))?;
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
                            CommandOutcome::Rejected(
                                CommandRejection::InvalidValue | CommandRejection::NotAdaptive,
                            ) => ErrorCode::InvalidInput,
                            CommandOutcome::Rejected(_) => ErrorCode::Unsupported,
                            _ => ErrorCode::Cancelled,
                        },
                        "command",
                        match outcome {
                            CommandOutcome::Rejected(reason) => reason.to_string(),
                            _ => format!("{outcome:?}"),
                        },
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
            let sleeping = self.backend.sleeping();
            if sleeping || active.envelope.suspend_epoch != self.backend.suspend_epoch() {
                let written = active.envelope.dispatched.load(Ordering::SeqCst);
                active.pending.cancel();
                let response = Response::failed(
                    active.envelope.request.id,
                    DaemonError::new(
                        if written {
                            ErrorCode::OutcomeUnknown
                        } else {
                            ErrorCode::Cancelled
                        },
                        "suspend",
                        "request interrupted by suspend; no mutation will be replayed",
                    ),
                );
                if sleeping {
                    self.handle_suspend();
                } else {
                    self.publish();
                }
                let _ = active.envelope.reply.send(response);
                return;
            }
            match action {
                CommandAction::Wait => {
                    self.active = Some(active);
                    self.publish();
                    return;
                }
                CommandAction::Send(packet) => {
                    // The pure action may have been produced before cancellation, shutdown
                    // or scheduler delay. Revalidate at the actual write boundary.
                    if self.stopped.load(Ordering::SeqCst) || active.envelope.client_cancelled() {
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
                CommandAction::ReopenForReadback { generation } => {
                    action = active
                        .pending
                        .mark_readback_reopened(generation, self.now());
                    if action != CommandAction::Wait {
                        continue;
                    }
                    let readback = ReadbackConnection {
                        address: self.state.bluetooth_address.clone(),
                        generation,
                        deadline: active.envelope.deadline,
                    };
                    self.active = Some(active);
                    self.release_transport();
                    self.publish();
                    let result = self.connect_bounded(Some(&readback));
                    let Some(mut resumed) = self.active.take() else {
                        return;
                    };
                    action = match result {
                        Ok(()) => {
                            if resumed.envelope.client_cancelled()
                                || self.stopped.load(Ordering::SeqCst)
                                || self.backend.sleeping()
                            {
                                resumed.pending.cancel()
                            } else {
                                resumed.pending.observe_readback(&self.state, self.now())
                            }
                        }
                        Err(error) => {
                            // Keep the immediate cause in the correlated reply: a later
                            // successful recovery may clear the current status error.
                            resumed.pending.transport_lost();
                            self.disconnect();
                            self.error(error.clone());
                            self.schedule_retry();
                            let _ = resumed.envelope.reply.send(Response::failed(
                                resumed.envelope.request.id,
                                DaemonError::new(ErrorCode::OutcomeUnknown,error.stage,
                                    format!("setting was written once but readback failed ({:?}): {}; inspect status before retrying",error.code,error.message)),
                            ));
                            return;
                        }
                    };
                    active = resumed;
                }
                CommandAction::Complete(outcome) => {
                    let uncertain = matches!(outcome, CommandOutcome::OutcomeUnknown);
                    let response = self.action_response(
                        active.envelope.request.id,
                        CommandAction::Complete(outcome),
                    );
                    if uncertain {
                        self.disconnect();
                        self.schedule_retry();
                    }
                    self.publish();
                    // A completed request must never race an older public snapshot.
                    let _ = active.envelope.reply.send(response);
                    return;
                }
            }
        }
    }
    fn enqueue(&mut self, envelope: Envelope) {
        if self.backend.sleeping()
            || self.recovery.state == Lifecycle::Suspended
            || envelope.suspend_epoch != self.backend.suspend_epoch()
        {
            let _ = envelope.reply.send(Response::failed(
                envelope.request.id,
                DaemonError::new(
                    ErrorCode::Cancelled,
                    "suspend",
                    "request cannot cross suspend",
                ),
            ));
            return;
        }
        if envelope.client_cancelled() {
            envelope.cancel_before_dispatch();
            return;
        }
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
        if self.backend.sleeping() || self.recovery.state == Lifecycle::Suspended {
            return;
        }
        if self.active.is_some() || self.key_exchange.is_some() {
            return;
        }
        let Some(envelope) = self.queue.pop_front() else {
            return;
        };
        if envelope.suspend_epoch != self.backend.suspend_epoch() {
            let _ = envelope.reply.send(Response::failed(
                envelope.request.id,
                DaemonError::new(
                    ErrorCode::Cancelled,
                    "suspend",
                    "request was admitted before a completed suspend cycle",
                ),
            ));
            return;
        }
        if envelope.client_cancelled() {
            envelope.cancel_before_dispatch();
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
            Operation::Doctor => Response::ok(id, self.status.doctor()),
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
                    if envelope.client_cancelled() || self.stopped.load(Ordering::SeqCst) {
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
                    if self.backend.sleeping() {
                        let _ = envelope.reply.send(Response::failed(
                            id,
                            DaemonError::new(
                                ErrorCode::Cancelled,
                                "suspend",
                                "key request interrupted before dispatch",
                            ),
                        ));
                        self.handle_suspend();
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
            exchange.envelope.client_cancelled() || Instant::now() >= exchange.envelope.deadline
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
            if sleeping {
                self.handle_suspend();
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
                let action = if active.envelope.client_cancelled() {
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
                            if self.backend.sleeping() {
                                self.handle_suspend();
                                continue;
                            }
                            if let Some(mut exchange) = self.key_exchange.take() {
                                if !exchange.envelope.client_cancelled()
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
                                let mut action =
                                    active
                                        .pending
                                        .on_packet(&packet, self.generation, self.now());
                                if action == CommandAction::Wait {
                                    action =
                                        active.pending.observe_readback(&self.state, self.now());
                                }
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
            handled_suspend_epoch: 0,
        }
    }
    #[test]
    fn readable_open_peer_remains_live_and_probe_does_not_consume_pipelined_bytes() {
        use std::{
            io::{Read, Write},
            os::fd::AsFd,
        };
        let (mut server, mut client) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = server.as_fd().try_clone_to_owned().unwrap();
        client.write_all(b"next request\n").unwrap();
        assert!(peer_connected(&peer));
        let mut buffer = [0; 13];
        server.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"next request\n");
        // SAFETY: this call only reads descriptor flags from the live owned fd.
        assert_ne!(
            unsafe { libc::fcntl(peer.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(!peer_connected(&peer));
    }
    #[cfg(feature = "ble")]
    #[test]
    fn unscheduled_eof_cancels_queued_key_request_even_while_reply_receiver_is_open() {
        use std::{io::Write, os::fd::AsFd};
        let transport = Arc::new(Spy(AtomicUsize::new(0)));
        let mut owner = owner(transport.clone());
        owner.status.connected = true;
        let private = tempfile::tempdir().unwrap();
        let root = private.path();
        owner.paths = Some(crate::storage::Paths {
            runtime: root.join("runtime"),
            state: root.join("state"),
            config: root.join("config"),
            legacy_state: root.join("legacy-state"),
            legacy_socket: root.join("legacy-socket"),
        });
        let (server, mut client) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = Arc::new(server.as_fd().try_clone_to_owned().unwrap());
        let (reply, mut response) = oneshot::channel();
        let dispatched = Arc::new(AtomicBool::new(false));
        owner.enqueue(Envelope {
            suspend_epoch: 0,
            peer: Some(peer),
            target_address: None,
            request: Request {
                version: 1,
                id: 1,
                timeout_ms: 1000,
                operation: Operation::FetchKeys,
            },
            deadline: Instant::now() + Duration::from_secs(1),
            reply,
            dispatched: dispatched.clone(),
        });
        assert_eq!(owner.queue.len(), 1);
        client.write_all(b"unread pipeline\n").unwrap();
        drop(client);
        owner.dispatch();
        assert_eq!(
            response.try_recv().unwrap().error.unwrap().code,
            ErrorCode::Cancelled
        );
        assert_eq!(transport.0.load(Ordering::SeqCst), 0);
        assert!(!dispatched.load(Ordering::SeqCst));
        assert!(owner.key_exchange.is_none());
    }
    #[test]
    fn expired_cancelled_or_stopping_requests_cannot_write_after_a_pure_send_action() {
        for reason in 0..6 {
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
            let (peer, _kept_client) = if reason >= 4 {
                use std::{io::Write, os::fd::AsFd};
                let (server, mut client) = std::os::unix::net::UnixStream::pair().unwrap();
                let peer = Arc::new(server.as_fd().try_clone_to_owned().unwrap());
                if reason == 5 {
                    client.write_all(b"queued next request\n").unwrap();
                    client.shutdown(std::net::Shutdown::Write).unwrap();
                    (Some(peer), Some(client))
                } else {
                    drop(client);
                    (Some(peer), None)
                }
            } else {
                (None, None)
            };
            let dispatched = Arc::new(AtomicBool::new(false));
            let envelope = Envelope {
                suspend_epoch: 0,
                peer,
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
    #[derive(Default)]
    struct SuspendFacts {
        epoch: std::sync::atomic::AtomicU64,
        acknowledged: std::sync::atomic::AtomicU64,
        actual: AtomicBool,
        block_once: AtomicBool,
        entered: AtomicBool,
        release: Mutex<bool>,
        wake: std::sync::Condvar,
        cycle_on_sleep_read: AtomicUsize,
        sleep_reads: AtomicUsize,
        audio_started: AtomicUsize,
    }
    struct SuspendBackend(Arc<SuspendFacts>);
    impl Backend for SuspendBackend {
        fn discover(&self, _: Option<&str>) -> Result<crate::backend::Endpoint, DaemonError> {
            Err(DaemonError::transient(
                ErrorCode::DeviceUnavailable,
                "fake discovery",
                "not connected",
            ))
        }
        fn connect(
            &self,
            _: &crate::backend::Endpoint,
            _: Duration,
        ) -> Result<Arc<dyn SessionTransport>, DaemonError> {
            panic!("suspend fixture must not open Bluetooth")
        }
        fn sleeping(&self) -> bool {
            if self.0.block_once.swap(false, Ordering::SeqCst) {
                self.0.entered.store(true, Ordering::SeqCst);
                let mut released = self.0.release.lock().unwrap();
                while !*released {
                    released = self.0.wake.wait(released).unwrap();
                }
            }
            let current = self.0.actual.load(Ordering::SeqCst)
                || self.suspend_epoch() != self.0.acknowledged.load(Ordering::SeqCst);
            let read = self.0.sleep_reads.fetch_add(1, Ordering::SeqCst) + 1;
            if self.0.cycle_on_sleep_read.load(Ordering::SeqCst) == read {
                let next = self.0.epoch.fetch_add(1, Ordering::SeqCst) + 1;
                self.0.acknowledged.store(next, Ordering::SeqCst);
            }
            current
        }
        fn suspend_epoch(&self) -> u64 {
            self.0.epoch.load(Ordering::SeqCst)
        }
        fn acknowledge_suspend(&self, epoch: u64) {
            self.0
                .acknowledged
                .fetch_max(epoch.min(self.suspend_epoch()), Ordering::SeqCst);
        }
        fn audio_async(&self, _: String, _: Operation) -> crate::backend::AudioFuture {
            let facts = self.0.clone();
            Box::pin(async move {
                facts.audio_started.fetch_add(1, Ordering::SeqCst);
                let next = facts.epoch.fetch_add(1, Ordering::SeqCst) + 1;
                facts.acknowledged.store(next, Ordering::SeqCst);
                Ok(serde_json::json!({"old_audio_result":true}))
            })
        }
    }
    fn sleep_envelope(epoch: u64, id: u64) -> (Envelope, oneshot::Receiver<Response>) {
        let (reply, receiver) = oneshot::channel();
        (
            Envelope {
                suspend_epoch: epoch,
                peer: None,
                target_address: None,
                request: Request {
                    version: 1,
                    id,
                    timeout_ms: 5000,
                    operation: Operation::Change(SettingChange::ListeningMode(
                        NoiseControlMode::NoiseCancellation,
                    )),
                },
                deadline: Instant::now() + Duration::from_secs(5),
                reply,
                dispatched: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }
    struct IdleSpy(AtomicUsize);
    impl SessionTransport for IdleSpy {
        fn send(&self, _: &[u8], _: Duration) -> Result<(), TransportError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn recv(&self, _: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
            std::thread::sleep(timeout.min(Duration::from_millis(5)));
            Err(TransportError::Timeout)
        }
        fn close(&self) {}
    }
    struct RunningOwner {
        stopped: Arc<AtomicBool>,
        facts: Arc<SuspendFacts>,
        join: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for RunningOwner {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            *self.facts.release.lock().unwrap() = true;
            self.facts.wake.notify_all();
            if let Some(join) = self.join.take() {
                join.join().unwrap();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_owner_discards_channel_pending_mutations_after_unpolled_sleep_cycle() {
        let facts = Arc::new(SuspendFacts::default());
        facts.block_once.store(true, Ordering::SeqCst);
        let transport = Arc::new(IdleSpy(AtomicUsize::new(0)));
        let mut worker = owner(Arc::new(Spy(AtomicUsize::new(0))));
        worker.backend = Arc::new(SuspendBackend(facts.clone()));
        worker.status.connected = true;
        worker.connection = Some(transport.clone());
        worker.transport = Arc::new(Mutex::new(Some(transport.clone())));
        let (sender, receiver) = mpsc::sync_channel(QUEUE_LIMIT);
        worker.receiver = receiver;
        let (already_queued, already_queued_reply) = sleep_envelope(0, 0);
        worker.queue.push_back(already_queued);
        let stopped = worker.stopped.clone();
        let mut running = RunningOwner {
            stopped,
            facts: facts.clone(),
            join: Some(std::thread::spawn(move || worker.run())),
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !facts.entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (first, first_reply) = sleep_envelope(0, 1);
        let (second, second_reply) = sleep_envelope(0, 2);
        sender
            .try_send(first)
            .unwrap_or_else(|_| panic!("first admitted request"));
        sender
            .try_send(second)
            .unwrap_or_else(|_| panic!("second admitted request"));
        // Complete true→false while the owner has not polled either edge.
        facts.epoch.store(1, Ordering::SeqCst);
        facts.actual.store(false, Ordering::SeqCst);
        *facts.release.lock().unwrap() = true;
        facts.wake.notify_all();
        for reply in [already_queued_reply, first_reply, second_reply] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), reply)
                    .await
                    .unwrap()
                    .unwrap()
                    .error
                    .unwrap()
                    .code,
                ErrorCode::Cancelled
            );
        }
        assert_eq!(
            transport.0.load(Ordering::SeqCst),
            0,
            "pre-suspend requests must never reach the existing transport"
        );
        assert_eq!(facts.acknowledged.load(Ordering::SeqCst), 1);
        running.stopped.store(true, Ordering::SeqCst);
        running.join.take().unwrap().join().unwrap();
    }

    #[test]
    fn acknowledged_cycles_reject_late_admission_at_enqueue_dispatch_and_write() {
        for boundary in 0..3 {
            let facts = Arc::new(SuspendFacts::default());
            facts.epoch.store(1, Ordering::SeqCst);
            facts.acknowledged.store(1, Ordering::SeqCst);
            let transport = Arc::new(Spy(AtomicUsize::new(0)));
            let mut worker = owner(transport.clone());
            worker.status.connected = true;
            worker.backend = Arc::new(SuspendBackend(facts));
            let (envelope, mut reply) = sleep_envelope(0, 1);
            match boundary {
                0 => worker.enqueue(envelope),
                1 => {
                    worker.queue.push_back(envelope);
                    worker.dispatch();
                }
                _ => {
                    let mut pending = PendingChange::new(
                        SettingChange::ListeningMode(NoiseControlMode::NoiseCancellation),
                        1,
                        10_000,
                    );
                    let action = pending.begin(&worker.state, 0);
                    worker.handle_action(Active { envelope, pending }, action);
                }
            }
            assert_eq!(
                reply.try_recv().unwrap().error.unwrap().code,
                ErrorCode::Cancelled
            );
            assert_eq!(transport.0.load(Ordering::SeqCst), 0);
            assert!(
                worker.state.connected,
                "rejecting an old request must not tear down a healthy resumed session"
            );
        }
    }

    #[test]
    fn suspend_ack_keeps_actual_sleep_and_handles_a_new_epoch_while_already_suspended() {
        let facts = Arc::new(SuspendFacts::default());
        facts.epoch.store(1, Ordering::SeqCst);
        facts.actual.store(true, Ordering::SeqCst);
        let backend = Arc::new(SuspendBackend(facts.clone()));
        let mut worker = owner(Arc::new(Spy(AtomicUsize::new(0))));
        worker.backend = backend.clone();
        worker.handle_suspend();
        let first_generation = worker.recovery.generation;
        assert!(backend.sleeping());
        assert!(!worker.state.connected);
        worker.handle_suspend();
        assert_eq!(
            worker.recovery.generation, first_generation,
            "same epoch is idempotent"
        );
        facts.epoch.store(2, Ordering::SeqCst);
        facts.actual.store(false, Ordering::SeqCst);
        assert!(backend.sleeping());
        worker.handle_suspend();
        assert!(worker.recovery.generation > first_generation);
        assert_eq!(facts.acknowledged.load(Ordering::SeqCst), 2);
        assert!(!backend.sleeping());
        assert_eq!(
            worker.recovery.state,
            Lifecycle::Suspended,
            "resume is a separate later owner iteration"
        );
    }

    #[tokio::test]
    async fn audio_success_after_an_acknowledged_cycle_is_never_published_as_current() {
        for (operation, before_dispatch) in [
            (Operation::AudioStatus, false),
            (Operation::Unmute, false),
            (Operation::AudioStatus, true),
            (Operation::Unmute, true),
        ] {
            let facts = Arc::new(SuspendFacts::default());
            if before_dispatch {
                facts.cycle_on_sleep_read.store(2, Ordering::SeqCst);
            }
            let (sender, _receiver) = mpsc::sync_channel(QUEUE_LIMIT);
            let (status_tx, status) =
                watch::channel(Snapshot::new("audio-epoch".into(), &DeviceState::default()));
            let (audio_feed, _) = watch::channel(serde_json::Value::Null);
            let (publication_health, _) = watch::channel(None);
            let (manual_overrides, _) = watch::channel(0);
            let handle = SessionHandle {
                sender,
                backend: Arc::new(SuspendBackend(facts.clone())),
                audio_feed,
                publication_health,
                manual_overrides,
                status,
                stopped: Arc::new(AtomicBool::new(false)),
                transport: Arc::new(Mutex::new(None)),
            };
            let readonly = matches!(operation, Operation::AudioStatus);
            let response = handle
                .request(Request {
                    version: 1,
                    id: 1,
                    timeout_ms: 1000,
                    operation,
                })
                .await;
            assert_eq!(
                response.error.unwrap().code,
                if readonly || before_dispatch {
                    ErrorCode::Cancelled
                } else {
                    ErrorCode::OutcomeUnknown
                }
            );
            assert_eq!(*handle.audio_feed.borrow(), serde_json::Value::Null);
            assert_eq!(
                facts.audio_started.load(Ordering::SeqCst),
                usize::from(!before_dispatch)
            );
            drop(status_tx);
        }
    }
    #[test]
    fn suspend_cancels_written_active_as_unknown_without_replaying_it() {
        let facts = Arc::new(SuspendFacts::default());
        let transport = Arc::new(Spy(AtomicUsize::new(0)));
        let mut worker = owner(transport.clone());
        worker.backend = Arc::new(SuspendBackend(facts.clone()));
        let (envelope, mut reply) = sleep_envelope(0, 1);
        let mut pending = PendingChange::new(
            SettingChange::ListeningMode(NoiseControlMode::NoiseCancellation),
            1,
            10_000,
        );
        let action = pending.begin(&worker.state, 0);
        worker.handle_action(Active { envelope, pending }, action);
        assert!(worker.active.is_some());
        assert_eq!(transport.0.load(Ordering::SeqCst), 1);
        facts.epoch.store(1, Ordering::SeqCst);
        worker.handle_suspend();
        assert_eq!(
            reply.try_recv().unwrap().error.unwrap().code,
            ErrorCode::OutcomeUnknown
        );
        assert_eq!(
            transport.0.load(Ordering::SeqCst),
            1,
            "suspend must not resend an already written mutation"
        );
        assert!(worker.active.is_none());
        assert!(!worker.state.connected);
        assert_eq!(facts.acknowledged.load(Ordering::SeqCst), 1);
    }
    #[cfg(feature = "ble")]
    #[test]
    fn suspend_discards_an_inflight_key_exchange_without_persisting_or_replaying() {
        let facts = Arc::new(SuspendFacts::default());
        let transport = Arc::new(Spy(AtomicUsize::new(0)));
        let mut worker = owner(transport.clone());
        worker.status.connected = true;
        worker.backend = Arc::new(SuspendBackend(facts.clone()));
        let root = tempfile::tempdir().unwrap();
        let paths = crate::storage::Paths {
            runtime: root.path().join("run"),
            state: root.path().join("state"),
            config: root.path().join("config"),
            legacy_state: root.path().join("legacy"),
            legacy_socket: root.path().join("old.sock"),
        };
        worker.paths = Some(paths.clone());
        worker.telemetry = Telemetry::new(worker.started, Some(paths.clone()));
        let (mut envelope, mut reply) = sleep_envelope(0, 1);
        envelope.request.operation = Operation::FetchKeys;
        worker.enqueue(envelope);
        worker.dispatch();
        assert!(worker.key_exchange.is_some());
        assert_eq!(transport.0.load(Ordering::SeqCst), 1);
        facts.epoch.store(1, Ordering::SeqCst);
        worker.handle_suspend();
        assert_eq!(
            reply.try_recv().unwrap().error.unwrap().code,
            ErrorCode::Cancelled
        );
        assert!(worker.key_exchange.is_none());
        assert!(!paths.state.join("ble-keys.json").exists());
        assert_eq!(
            transport.0.load(Ordering::SeqCst),
            1,
            "key request must not be replayed across suspend"
        );
    }
}
