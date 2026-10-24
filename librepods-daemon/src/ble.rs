// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Optional scan ownership, isolated from the AACP session and command queue.

use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use librepods_core::device::ble_advert::BleAdvertData;
use librepods_core::transport::RawBleAdvertisement;
#[cfg(any(feature = "ble", test))]
use librepods_core::transport::TransportError;
use serde::{Deserialize, Serialize};

#[cfg(feature = "ble")]
use crate::keys::address_bytes;
use crate::keys::BleKeys;
use crate::{DaemonError, ErrorCode};

const EVENT_CAPACITY: usize = 32;
const READ_SLICE: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BleHealth {
    #[default]
    Disabled,
    Paused,
    MissingKeys,
    Starting,
    Scanning,
    Backoff,
    Stopped,
}

#[derive(Debug)]
pub enum BleEvent {
    Health {
        generation: u64,
        health: BleHealth,
    },
    Observation {
        generation: u64,
        observed_at_ms: u64,
        advertisement: BleAdvertData,
    },
}

struct TaggedEvent {
    revision: u64,
    event: BleEvent,
}

/// Injectable raw source; identity and protocol decoding are always done in core.
pub trait ScanSource: Send {
    fn next(&mut self, timeout: Duration) -> Result<Option<RawBleAdvertisement>, DaemonError>;
    /// Stop this client's scan; Drop must still release all owned resources on error.
    fn stop(&mut self) -> Result<(), DaemonError> {
        Ok(())
    }
}

pub trait ScanFactory: Send + Sync + 'static {
    fn start(&self) -> Result<Box<dyn ScanSource>, DaemonError>;
}

/// Native scan errors never contain protocol keys: scanners receive only raw OS advertisements.
/// Keep a bounded cause chain for diagnosis, preserving machine-readable classification.
#[cfg(any(feature = "ble", test))]
fn scan_error(stage: &str, error: TransportError) -> DaemonError {
    use librepods_linux::error::{kind, ErrorKind};
    let code = match kind(&error) {
        Some(ErrorKind::PermissionDenied) => ErrorCode::PermissionDenied,
        Some(ErrorKind::Timeout) => ErrorCode::CommandTimeout,
        Some(ErrorKind::Cancelled) => ErrorCode::Cancelled,
        Some(ErrorKind::Unsupported) => ErrorCode::Unsupported,
        Some(ErrorKind::InvalidInput) => ErrorCode::InvalidInput,
        Some(ErrorKind::Truncated) => ErrorCode::MalformedFrame,
        Some(ErrorKind::Io) => ErrorCode::ServiceUnavailable,
        _ => ErrorCode::ServiceUnavailable,
    };
    let mut causes = vec![error.to_string()];
    let mut source = std::error::Error::source(&error);
    for _ in 0..6 {
        let Some(cause) = source else {
            break;
        };
        causes.push(cause.to_string());
        source = cause.source();
    }
    let message: String = causes
        .join(": ")
        .chars()
        .filter(|character| !character.is_control())
        .take(1024)
        .collect();
    let mut result = DaemonError::new(code, stage, message);
    result.retryable = matches!(
        code,
        ErrorCode::ServiceUnavailable | ErrorCode::CommandTimeout
    );
    result
}

struct NativeFactory;

#[cfg(feature = "ble")]
struct NativeSource(librepods_linux::ble::BtleplugScanner);

#[cfg(feature = "ble")]
impl ScanSource for NativeSource {
    fn next(&mut self, timeout: Duration) -> Result<Option<RawBleAdvertisement>, DaemonError> {
        self.0
            .next_advertisement_timeout(timeout)
            .map_err(|error| scan_error("BLE receive", error))
    }
    fn stop(&mut self) -> Result<(), DaemonError> {
        self.0
            .shutdown_checked()
            .map_err(|error| scan_error("BLE stop", error))
    }
}

impl ScanFactory for NativeFactory {
    fn start(&self) -> Result<Box<dyn ScanSource>, DaemonError> {
        #[cfg(feature = "ble")]
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| DaemonError::io("BLE coordinator runtime", error))?;
            let scanner = runtime
                .block_on(librepods_linux::ble::BtleplugScanner::start())
                .map_err(|error| scan_error("BLE startup", error))?;
            Ok(Box::new(NativeSource(scanner)))
        }
        #[cfg(not(feature = "ble"))]
        Err(DaemonError::new(
            ErrorCode::Unsupported,
            "BLE scan",
            "BLE support is not compiled in",
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Policy {
    enabled: bool,
    allowed: bool,
    generation: u64,
    keys: Option<BleKeys>,
}

struct Shared {
    policy: Policy,
    revision: u64,
    stopped: bool,
    health: BleHealth,
    last_error: Option<DaemonError>,
    quiescent_revision: u64,
}

type Control = Arc<(Mutex<Shared>, Condvar)>;

pub struct BleCoordinator {
    control: Control,
    events: Mutex<mpsc::Receiver<TaggedEvent>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl BleCoordinator {
    pub fn start(origin: Instant) -> Result<Self, DaemonError> {
        Self::start_with_factory(origin, Arc::new(NativeFactory))
    }

    pub fn start_with_factory(
        origin: Instant,
        factory: Arc<dyn ScanFactory>,
    ) -> Result<Self, DaemonError> {
        let control = Arc::new((
            Mutex::new(Shared {
                policy: Policy {
                    enabled: false,
                    allowed: false,
                    generation: 0,
                    keys: None,
                },
                revision: 0,
                stopped: false,
                health: BleHealth::Disabled,
                last_error: None,
                quiescent_revision: 0,
            }),
            Condvar::new(),
        ));
        let (sender, receiver) = mpsc::sync_channel(EVENT_CAPACITY);
        let shared = control.clone();
        let worker = thread::Builder::new()
            .name("librepods-ble-policy".into())
            .spawn(move || run(shared, sender, factory, origin))
            .map_err(|error| DaemonError::io("start BLE coordinator", error))?;
        Ok(Self {
            control,
            events: Mutex::new(receiver),
            worker: Some(worker),
        })
    }

    pub fn configure(
        &self,
        enabled: bool,
        allowed: bool,
        generation: u64,
        keys: Option<BleKeys>,
    ) -> Result<(), DaemonError> {
        if enabled && !cfg!(feature = "ble") {
            return Err(DaemonError::new(
                ErrorCode::Unsupported,
                "BLE configuration",
                "BLE support is not compiled in",
            ));
        }
        let policy = Policy {
            enabled,
            allowed,
            generation,
            keys,
        };
        let (lock, wake) = &*self.control;
        let mut shared = lock.lock().map_err(|_| {
            DaemonError::new(
                ErrorCode::Internal,
                "BLE configuration",
                "coordinator lock poisoned",
            )
        })?;
        if shared.stopped {
            return Err(DaemonError::new(
                ErrorCode::Cancelled,
                "BLE configuration",
                "coordinator stopped",
            ));
        }
        if shared.policy != policy {
            shared.policy = policy;
            shared.revision = shared.revision.wrapping_add(1);
            wake.notify_all();
        }
        Ok(())
    }

    /// Disable scanning and wait until the source has stopped and dropped before AACP connects.
    /// A timeout never grants permission to connect concurrently with an active scan.
    pub fn pause_for_connection(&self, timeout: Duration) -> Result<(), DaemonError> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            DaemonError::new(
                ErrorCode::InvalidInput,
                "BLE pause",
                "timeout exceeds clock range",
            )
        })?;
        let (lock, wake) = &*self.control;
        let mut shared = lock.lock().unwrap_or_else(|error| error.into_inner());
        if shared.stopped {
            return Err(DaemonError::new(
                ErrorCode::Cancelled,
                "BLE pause",
                "coordinator stopped",
            ));
        }
        if shared.policy.allowed {
            shared.policy.allowed = false;
            shared.revision = shared.revision.wrapping_add(1);
            wake.notify_all();
        }
        let revision = shared.revision;
        loop {
            if shared.stopped || shared.revision != revision {
                return Err(DaemonError::new(
                    ErrorCode::Cancelled,
                    "BLE pause",
                    "pause superseded",
                ));
            }
            if shared.quiescent_revision == revision {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DaemonError::transient(
                    ErrorCode::CommandTimeout,
                    "BLE pause",
                    "scan source has not acknowledged stopping",
                ));
            }
            shared = wake
                .wait_timeout(shared, remaining)
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
    }

    pub fn health(&self) -> BleHealth {
        self.control
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .health
    }

    /// Authoritative diagnostics remain available even when event consumers lag.
    pub fn last_error(&self) -> Option<DaemonError> {
        self.control
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_error
            .clone()
    }

    pub fn next_event_timeout(&self, timeout: Duration) -> Result<Option<BleEvent>, DaemonError> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            DaemonError::new(
                ErrorCode::InvalidInput,
                "BLE events",
                "timeout exceeds clock range",
            )
        })?;
        let receiver = self.events.lock().map_err(|_| {
            DaemonError::new(ErrorCode::Internal, "BLE events", "event lock poisoned")
        })?;
        loop {
            let event = if timeout.is_zero() {
                match receiver.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => return Ok(None),
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(None),
                }
            } else {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                match receiver.recv_timeout(remaining) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
                }
            };
            let revision = self
                .control
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .revision;
            if event.revision == revision {
                return Ok(Some(event.event));
            }
        }
    }

    pub fn shutdown(&mut self) {
        {
            let (lock, wake) = &*self.control;
            let mut shared = lock.lock().unwrap_or_else(|error| error.into_inner());
            shared.stopped = true;
            shared.revision = shared.revision.wrapping_add(1);
            wake.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for BleCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn state(control: &Control) -> (Policy, u64, bool) {
    let shared = control.0.lock().unwrap_or_else(|error| error.into_inner());
    (shared.policy.clone(), shared.revision, shared.stopped)
}

fn health(
    control: &Control,
    sender: &mpsc::SyncSender<TaggedEvent>,
    revision: u64,
    generation: u64,
    health: BleHealth,
) {
    let mut shared = control.0.lock().unwrap_or_else(|error| error.into_inner());
    if shared.revision != revision || shared.health == health {
        return;
    }
    shared.health = health;
    let _ = sender.try_send(TaggedEvent {
        revision,
        event: BleEvent::Health { generation, health },
    });
}

fn acknowledge_quiescence(control: &Control, revision: u64) {
    let (lock, wake) = &**control;
    let mut shared = lock.lock().unwrap_or_else(|error| error.into_inner());
    if shared.revision == revision {
        shared.quiescent_revision = revision;
        wake.notify_all();
    }
}

fn record_error(control: &Control, revision: u64, error: DaemonError) {
    let mut shared = control.0.lock().unwrap_or_else(|error| error.into_inner());
    if shared.revision == revision && !shared.stopped {
        shared.last_error = Some(error);
    }
}

fn wait(control: &Control, revision: u64, timeout: Option<Duration>) {
    let (lock, wake) = &**control;
    let shared = lock.lock().unwrap_or_else(|error| error.into_inner());
    if shared.stopped || shared.revision != revision {
        return;
    }
    if let Some(timeout) = timeout {
        drop(
            wake.wait_timeout(shared, timeout)
                .unwrap_or_else(|error| error.into_inner()),
        );
    } else {
        drop(wake.wait(shared).unwrap_or_else(|error| error.into_inner()));
    }
}

#[cfg(feature = "ble")]
fn verified(raw: &RawBleAdvertisement, keys: &BleKeys) -> Option<BleAdvertData> {
    let address = address_bytes(&raw.address).ok()?;
    librepods_core::device::ble_advert::decode_verified_advertisement(
        &raw.manufacturer_data,
        keys.irk(),
        keys.encryption_key(),
        &address,
    )
}

#[cfg(not(feature = "ble"))]
fn verified(_raw: &RawBleAdvertisement, _keys: &BleKeys) -> Option<BleAdvertData> {
    None
}

fn release_source(control: &Control, revision: u64, scanner: &mut Option<Box<dyn ScanSource>>) {
    if let Some(mut source) = scanner.take() {
        if let Err(error) = source.stop() {
            record_error(control, revision, error);
        }
        // The native worker and its BlueZ client are gone before quiescence is acknowledged.
        drop(source);
    }
}

fn run(
    control: Control,
    sender: mpsc::SyncSender<TaggedEvent>,
    factory: Arc<dyn ScanFactory>,
    origin: Instant,
) {
    let mut scanner: Option<Box<dyn ScanSource>> = None;
    let mut active_revision = u64::MAX;
    let mut retry_delay = Duration::from_secs(1);
    let mut healthy_since = Instant::now();
    loop {
        let (policy, revision, stopped) = state(&control);
        if stopped {
            break;
        }
        if active_revision != revision {
            release_source(&control, revision, &mut scanner);
            retry_delay = Duration::from_secs(1);
            active_revision = revision;
        }
        let pause = if !policy.enabled {
            Some(BleHealth::Disabled)
        } else if !policy.allowed {
            Some(BleHealth::Paused)
        } else if policy.keys.is_none() {
            Some(BleHealth::MissingKeys)
        } else {
            None
        };
        if let Some(paused) = pause {
            release_source(&control, revision, &mut scanner);
            health(&control, &sender, revision, policy.generation, paused);
            acknowledge_quiescence(&control, revision);
            wait(&control, revision, None);
            continue;
        }
        if scanner.is_none() {
            health(
                &control,
                &sender,
                revision,
                policy.generation,
                BleHealth::Starting,
            );
            match factory.start() {
                Ok(source) => {
                    if state(&control).1 != revision {
                        drop(source);
                        continue;
                    }
                    scanner = Some(source);
                    healthy_since = Instant::now();
                    control
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .last_error = None;
                    health(
                        &control,
                        &sender,
                        revision,
                        policy.generation,
                        BleHealth::Scanning,
                    );
                }
                Err(error) => {
                    let retryable = error.retryable;
                    record_error(&control, revision, error);
                    health(
                        &control,
                        &sender,
                        revision,
                        policy.generation,
                        BleHealth::Backoff,
                    );
                    wait(&control, revision, retryable.then_some(retry_delay));
                    retry_delay = (retry_delay * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        }
        let Some(source) = scanner.as_mut() else {
            continue;
        };
        match source.next(READ_SLICE) {
            Ok(Some(raw)) => {
                if healthy_since.elapsed() >= MAX_BACKOFF {
                    retry_delay = Duration::from_secs(1);
                }
                if state(&control).1 != revision {
                    continue;
                }
                if let Some(advertisement) =
                    policy.keys.as_ref().and_then(|keys| verified(&raw, keys))
                {
                    let observed_at_ms = origin.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    let _ = sender.try_send(TaggedEvent {
                        revision,
                        event: BleEvent::Observation {
                            generation: policy.generation,
                            observed_at_ms,
                            advertisement,
                        },
                    });
                }
            }
            Ok(None) => {
                if healthy_since.elapsed() >= MAX_BACKOFF {
                    retry_delay = Duration::from_secs(1);
                }
            }
            Err(error) => {
                let retryable = error.retryable;
                record_error(&control, revision, error);
                release_source(&control, revision, &mut scanner);
                health(
                    &control,
                    &sender,
                    revision,
                    policy.generation,
                    BleHealth::Backoff,
                );
                wait(&control, revision, retryable.then_some(retry_delay));
                retry_delay = (retry_delay * 2).min(MAX_BACKOFF);
            }
        }
    }
    release_source(&control, state(&control).1, &mut scanner);
    let mut shared = control.0.lock().unwrap_or_else(|error| error.into_inner());
    shared.health = BleHealth::Stopped;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailingFactory(Arc<AtomicUsize>);
    impl ScanFactory for FailingFactory {
        fn start(&self) -> Result<Box<dyn ScanSource>, DaemonError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(DaemonError::new(
                ErrorCode::PermissionDenied,
                "fake scan",
                "injected permission denial",
            ))
        }
    }

    #[cfg(feature = "ble")]
    fn wait_health(coordinator: &BleCoordinator, expected: BleHealth) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while coordinator.health() != expected {
            assert!(
                Instant::now() < deadline,
                "expected {expected:?}, got {:?}",
                coordinator.health()
            );
            let _ = coordinator.next_event_timeout(Duration::from_millis(20));
        }
    }

    #[test]
    fn disabled_default_never_opens_a_scanner_and_shutdown_wakes_idle_worker() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut coordinator = BleCoordinator::start_with_factory(
            Instant::now(),
            Arc::new(FailingFactory(attempts.clone())),
        )
        .expect("coordinator");
        assert_eq!(coordinator.health(), BleHealth::Disabled);
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        coordinator.shutdown();
        assert_eq!(coordinator.health(), BleHealth::Stopped);
        assert!(coordinator.configure(false, false, 0, None).is_err());
    }

    #[cfg(not(feature = "ble"))]
    #[test]
    fn absent_feature_rejects_enabling_without_opening_hardware() {
        let mut coordinator = BleCoordinator::start(Instant::now()).expect("coordinator");
        assert_eq!(
            coordinator
                .configure(true, true, 1, None)
                .expect_err("feature absent")
                .code,
            ErrorCode::Unsupported
        );
        assert_eq!(coordinator.health(), BleHealth::Disabled);
        coordinator.shutdown();
    }

    #[cfg(feature = "ble")]
    struct FeedFactory(Arc<Mutex<mpsc::Receiver<RawBleAdvertisement>>>);
    #[cfg(feature = "ble")]
    struct FeedSource(Arc<Mutex<mpsc::Receiver<RawBleAdvertisement>>>);
    #[cfg(feature = "ble")]
    impl ScanFactory for FeedFactory {
        fn start(&self) -> Result<Box<dyn ScanSource>, DaemonError> {
            Ok(Box::new(FeedSource(self.0.clone())))
        }
    }
    #[cfg(feature = "ble")]
    impl ScanSource for FeedSource {
        fn next(&mut self, timeout: Duration) -> Result<Option<RawBleAdvertisement>, DaemonError> {
            match self.0.lock().expect("test source").recv_timeout(timeout) {
                Ok(event) => Ok(Some(event)),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(DaemonError::new(
                    ErrorCode::ServiceUnavailable,
                    "fake scan",
                    "closed",
                )),
            }
        }
    }

    #[cfg(feature = "ble")]
    #[test]
    fn verified_advertisements_are_delivered_without_keys_and_stale_events_are_discarded() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let mut coordinator = BleCoordinator::start_with_factory(
            Instant::now(),
            Arc::new(FeedFactory(Arc::new(Mutex::new(receiver)))),
        )
        .expect("coordinator");
        // Bluetooth ah reference vector; encryption payload is a synthetic AES block, not a real device capture.
        let mut irk = [
            0xec, 0x02, 0x34, 0xa3, 0x57, 0xc8, 0xad, 0x05, 0x34, 0x10, 0x10, 0xa6, 0x0a, 0x39,
            0x7d, 0x9b,
        ];
        irk.reverse();
        let keys = BleKeys::new("AA:BB:CC:DD:EE:FF", irk, [9; 16]).expect("keys");
        coordinator
            .configure(true, true, 7, Some(keys))
            .expect("enable");
        wait_health(&coordinator, BleHealth::Scanning);
        let frame = vec![
            7, 25, 1, 0x24, 0x20, 0x20, 0x65, 8, 0, 0, 4, 0xe0, 0x75, 0x57, 0xe7, 0x78, 0x87, 0x3e,
            0xcb, 0x3d, 0x99, 0x6c, 0xfe, 0x6b, 0x49, 0xce, 0x68,
        ];
        sender
            .send(RawBleAdvertisement {
                address: "00:00:00:00:00:00".into(),
                manufacturer_data: frame.clone(),
                rssi: None,
            })
            .expect("unverified frame");
        sender
            .send(RawBleAdvertisement {
                address: "70:81:94:0D:FB:AA".into(),
                manufacturer_data: frame.clone(),
                rssi: None,
            })
            .expect("verified frame");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < deadline, "verified observation missing");
            if let Some(BleEvent::Observation {
                generation,
                advertisement,
                ..
            }) = coordinator
                .next_event_timeout(Duration::from_millis(20))
                .expect("events")
            {
                assert_eq!(generation, 7);
                assert_eq!(advertisement.encrypted_left_battery, Some(42));
                break;
            }
        }
        // A policy revision suppresses queued data even if the reader and disable request race.
        sender
            .send(RawBleAdvertisement {
                address: "70:81:94:0D:FB:AA".into(),
                manufacturer_data: frame,
                rssi: None,
            })
            .expect("queued frame");
        coordinator
            .configure(false, false, 8, None)
            .expect("disable");
        wait_health(&coordinator, BleHealth::Disabled);
        while let Some(event) = coordinator
            .next_event_timeout(Duration::ZERO)
            .expect("drain")
        {
            assert!(!matches!(event, BleEvent::Observation { .. }));
        }
        coordinator.shutdown();
    }

    #[cfg(feature = "ble")]
    #[test]
    fn missing_keys_recovery_pause_and_permission_failure_are_independent_states() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut coordinator = BleCoordinator::start_with_factory(
            Instant::now(),
            Arc::new(FailingFactory(attempts.clone())),
        )
        .expect("coordinator");
        coordinator
            .configure(true, true, 1, None)
            .expect("enabled without keys");
        wait_health(&coordinator, BleHealth::MissingKeys);
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        let keys = BleKeys::new("AA:BB:CC:DD:EE:FF", [1; 16], [2; 16]).expect("keys");
        coordinator
            .configure(true, false, 1, Some(keys.clone()))
            .expect("recovery pause");
        wait_health(&coordinator, BleHealth::Paused);
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        coordinator
            .configure(true, true, 1, Some(keys))
            .expect("allow scan");
        wait_health(&coordinator, BleHealth::Backoff);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let error = coordinator
            .last_error()
            .expect("permission failure retained");
        assert_eq!(error.code, ErrorCode::PermissionDenied);
        assert!(!error.retryable);
        coordinator
            .configure(false, false, 1, None)
            .expect("disable during retry");
        wait_health(&coordinator, BleHealth::Disabled);
        coordinator.shutdown();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn native_error_classification_retains_bounded_os_causes() {
        for (error, code, retryable) in [
            (
                TransportError::PermissionDenied,
                ErrorCode::PermissionDenied,
                false,
            ),
            (TransportError::Timeout, ErrorCode::CommandTimeout, true),
            (TransportError::Unsupported, ErrorCode::Unsupported, false),
            (TransportError::Cancelled, ErrorCode::Cancelled, false),
            (
                TransportError::ChannelClosed,
                ErrorCode::ServiceUnavailable,
                true,
            ),
        ] {
            let result = scan_error("test scan", error);
            assert_eq!(result.code, code);
            assert_eq!(result.retryable, retryable);
        }
        let result = scan_error(
            "test scan",
            TransportError::with_source(
                "open manager",
                std::io::Error::from_raw_os_error(libc::EACCES),
            ),
        );
        assert_eq!(result.code, ErrorCode::PermissionDenied);
        assert!(result.message.contains("open manager"));
        assert!(result.message.contains("Permission denied"));
        let result = scan_error(
            "test scan",
            TransportError::new("\n".to_owned() + &"x".repeat(4000)),
        );
        assert!(result.message.len() <= 1024);
        assert!(!result.message.contains('\n'));
    }

    #[cfg(feature = "ble")]
    struct DroppingSource(Arc<AtomicUsize>);
    #[cfg(feature = "ble")]
    impl ScanSource for DroppingSource {
        fn next(&mut self, timeout: Duration) -> Result<Option<RawBleAdvertisement>, DaemonError> {
            std::thread::sleep(timeout.min(Duration::from_millis(5)));
            Ok(None)
        }
    }
    #[cfg(feature = "ble")]
    impl Drop for DroppingSource {
        fn drop(&mut self) {
            std::thread::sleep(Duration::from_millis(40));
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[cfg(feature = "ble")]
    struct DroppingFactory(Arc<AtomicUsize>);
    #[cfg(feature = "ble")]
    impl ScanFactory for DroppingFactory {
        fn start(&self) -> Result<Box<dyn ScanSource>, DaemonError> {
            Ok(Box::new(DroppingSource(self.0.clone())))
        }
    }
    #[cfg(feature = "ble")]
    #[test]
    fn connection_pause_acknowledges_source_drop_before_permitting_aacp() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut coordinator = BleCoordinator::start_with_factory(
            Instant::now(),
            Arc::new(DroppingFactory(drops.clone())),
        )
        .expect("coordinator");
        let keys = BleKeys::new("AA:BB:CC:DD:EE:FF", [1; 16], [2; 16]).expect("keys");
        coordinator
            .configure(true, true, 1, Some(keys.clone()))
            .expect("enable");
        wait_health(&coordinator, BleHealth::Scanning);
        assert_eq!(
            coordinator
                .pause_for_connection(Duration::ZERO)
                .expect_err("not yet quiescent")
                .code,
            ErrorCode::CommandTimeout
        );
        coordinator
            .pause_for_connection(Duration::from_secs(1))
            .expect("source stopped");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "ack must follow native StopDiscovery/drop"
        );
        assert_eq!(coordinator.health(), BleHealth::Paused);
        coordinator
            .configure(true, true, 1, Some(keys))
            .expect("resume optional scan");
        wait_health(&coordinator, BleHealth::Scanning);
        coordinator.shutdown();
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert_eq!(
            coordinator
                .pause_for_connection(Duration::ZERO)
                .expect_err("stopped")
                .code,
            ErrorCode::Cancelled
        );
    }
    #[cfg(feature = "ble")]
    struct RetrySource;
    #[cfg(feature = "ble")]
    impl ScanSource for RetrySource {
        fn next(&mut self, _: Duration) -> Result<Option<RawBleAdvertisement>, DaemonError> {
            Err(DaemonError::transient(
                ErrorCode::ServiceUnavailable,
                "fake receive",
                "injected scanner loss",
            ))
        }
    }
    #[cfg(feature = "ble")]
    struct RetryFactory(Arc<AtomicUsize>);
    #[cfg(feature = "ble")]
    impl ScanFactory for RetryFactory {
        fn start(&self) -> Result<Box<dyn ScanSource>, DaemonError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(RetrySource))
        }
    }
    #[cfg(feature = "ble")]
    #[test]
    fn immediate_source_failures_back_off_and_pause_interrupts_retry_wait() {
        let starts = Arc::new(AtomicUsize::new(0));
        let mut coordinator = BleCoordinator::start_with_factory(
            Instant::now(),
            Arc::new(RetryFactory(starts.clone())),
        )
        .expect("coordinator");
        let keys = BleKeys::new("AA:BB:CC:DD:EE:FF", [1; 16], [2; 16]).expect("keys");
        coordinator
            .configure(true, true, 1, Some(keys))
            .expect("enable");
        let deadline = Instant::now() + Duration::from_secs(3);
        while starts.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        wait_health(&coordinator, BleHealth::Backoff);
        let error = coordinator.last_error().expect("source error retained");
        assert_eq!(error.code, ErrorCode::ServiceUnavailable);
        assert!(error.retryable);
        assert_eq!(error.message, "injected scanner loss");
        // A second immediate source failure waits2s; recreating the source must not reset it to1s.
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        let paused = Instant::now();
        coordinator
            .pause_for_connection(Duration::from_secs(1))
            .expect("pause wakes retry");
        assert!(paused.elapsed() < Duration::from_millis(200));
        coordinator.shutdown();
    }
    #[cfg(feature = "ble")]
    struct StopFailSource(Arc<AtomicUsize>);
    #[cfg(feature = "ble")]
    impl ScanSource for StopFailSource {
        fn next(&mut self, timeout: Duration) -> Result<Option<RawBleAdvertisement>, DaemonError> {
            std::thread::sleep(timeout.min(Duration::from_millis(5)));
            Ok(None)
        }
        fn stop(&mut self) -> Result<(), DaemonError> {
            Err(DaemonError::transient(
                ErrorCode::CommandTimeout,
                "fake stop",
                "stop request timed out; client released by Drop",
            ))
        }
    }
    #[cfg(feature = "ble")]
    impl Drop for StopFailSource {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[cfg(feature = "ble")]
    struct StopFailFactory(Arc<AtomicUsize>);
    #[cfg(feature = "ble")]
    impl ScanFactory for StopFailFactory {
        fn start(&self) -> Result<Box<dyn ScanSource>, DaemonError> {
            Ok(Box::new(StopFailSource(self.0.clone())))
        }
    }
    #[cfg(feature = "ble")]
    #[test]
    fn failed_optional_stop_retains_diagnostic_but_released_client_allows_aacp() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut coordinator = BleCoordinator::start_with_factory(
            Instant::now(),
            Arc::new(StopFailFactory(drops.clone())),
        )
        .expect("coordinator");
        coordinator
            .configure(
                true,
                true,
                1,
                Some(BleKeys::new("AA:BB:CC:DD:EE:FF", [1; 16], [2; 16]).expect("keys")),
            )
            .expect("enable");
        wait_health(&coordinator, BleHealth::Scanning);
        coordinator
            .pause_for_connection(Duration::from_secs(1))
            .expect("client release is quiescence");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            coordinator
                .last_error()
                .expect("stop failure retained")
                .code,
            ErrorCode::CommandTimeout
        );
        assert_eq!(coordinator.health(), BleHealth::Paused);
        coordinator.shutdown();
    }
}
