// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Ordered MPRIS playback history. Overflow and bus loss invalidate ownership.

use crate::error::PlatformError;
use futures::StreamExt;
use librepods_core::transport::TransportError;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex, Notify};

const CAPACITY: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    Playback {
        bus_name: String,
        owner: String,
        playback_status: String,
    },
    Invalidate {
        reason: &'static str,
    },
}

struct Shared {
    ready: AtomicBool,
    epoch: AtomicU64,
    wake: Notify,
}
impl Shared {
    fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_one();
    }
}
struct Receiver {
    events: mpsc::Receiver<(u64, PlayerEvent)>,
    epoch: u64,
}

pub struct PlayerMonitor {
    receiver: Mutex<Receiver>,
    shared: Arc<Shared>,
    cancel: watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl PlayerMonitor {
    pub fn start() -> Result<Self, TransportError> {
        Self::start_with_bus(None)
    }
    pub fn start_at_bus(address: String) -> Result<Self, TransportError> {
        Self::start_with_bus(Some(address))
    }
    fn start_with_bus(address: Option<String>) -> Result<Self, TransportError> {
        let (sender, events) = mpsc::channel(CAPACITY);
        let (cancel, mut stop) = watch::channel(false);
        let shared = Arc::new(Shared {
            ready: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            wake: Notify::new(),
        });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new().name("librepods-players".into()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() { Ok(runtime) => runtime, Err(_) => { worker_shared.invalidate(); return; } };
            runtime.block_on(async {
                loop {
                    let listen = listen(address.as_deref(), &sender, &worker_shared);
                    tokio::select! { _ = stop.changed() => break, _ = listen => {} }
                    worker_shared.ready.store(false, Ordering::Release);
                    worker_shared.invalidate();
                    tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                }
                worker_shared.ready.store(false, Ordering::Release);
                worker_shared.invalidate();
            });
        }).map_err(|error| PlatformError::io("start MPRIS history monitor", error))?;
        Ok(Self {
            receiver: Mutex::new(Receiver { events, epoch: 0 }),
            shared,
            cancel,
            worker: Some(worker),
        })
    }
    pub fn is_ready(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }

    /// Cancellation-safe: a dropped wait never consumes a playback transition.
    pub async fn next_event(&self) -> Option<PlayerEvent> {
        let mut receiver = self.receiver.lock().await;
        loop {
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            if receiver.epoch != epoch {
                receiver.epoch = epoch;
                return Some(PlayerEvent::Invalidate {
                    reason: "playback history interrupted or player ownership changed",
                });
            }
            tokio::select! {
                _ = self.shared.wake.notified() => {},
                event = receiver.events.recv() => {
                    let (event_epoch, event) = event?;
                    if event_epoch == self.shared.epoch.load(Ordering::Acquire) { return Some(event); }
                }
            }
        }
    }
    /// Drain pending history before applying an automatic playback decision.
    pub fn try_next_event(&self) -> Option<PlayerEvent> {
        let mut receiver = self.receiver.try_lock().ok()?;
        loop {
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            if receiver.epoch != epoch {
                receiver.epoch = epoch;
                return Some(PlayerEvent::Invalidate {
                    reason: "playback history interrupted or player ownership changed",
                });
            }
            let (event_epoch, event) = receiver.events.try_recv().ok()?;
            if event_epoch == self.shared.epoch.load(Ordering::Acquire) {
                return Some(event);
            }
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.shared.ready.store(false, Ordering::Release);
    }
}
impl Drop for PlayerMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn listen(
    address: Option<&str>,
    sender: &mpsc::Sender<(u64, PlayerEvent)>,
    shared: &Shared,
) -> Result<(), TransportError> {
    let connection = tokio::time::timeout(Duration::from_secs(2), async {
        match address {
            Some(address) => zbus::connection::Builder::address(address)?.build().await,
            None => zbus::Connection::session().await,
        }
    })
    .await
    .map_err(|error| TransportError::with_source("connect MPRIS history bus", error))?
    .map_err(|error| TransportError::with_source("connect MPRIS history bus", error))?;
    let mut playback = zbus::MessageStream::for_match_rule("type='signal',path='/org/mpris/MediaPlayer2',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',arg0='org.mpris.MediaPlayer2.Player'", &connection, Some(128)).await.map_err(|error| TransportError::with_source("subscribe MPRIS playback history", error))?;
    let mut owners = zbus::MessageStream::for_match_rule("type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0namespace='org.mpris.MediaPlayer2'", &connection, Some(64)).await.map_err(|error| TransportError::with_source("subscribe MPRIS owner history", error))?;
    let mut names: HashMap<String, Vec<String>> = HashMap::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        let bus = zbus::fdo::DBusProxy::new(&connection).await?;
        for name in bus
            .list_names()
            .await?
            .into_iter()
            .filter(|name| name.as_str().starts_with("org.mpris.MediaPlayer2."))
            .take(64)
        {
            let Ok(bus_name) = name.as_str().try_into() else {
                continue;
            };
            if let Ok(owner) = bus.get_name_owner(bus_name).await {
                names
                    .entry(owner.to_string())
                    .or_default()
                    .push(name.to_string());
            }
        }
        Ok::<(), zbus::Error>(())
    })
    .await
    .map_err(|error| TransportError::with_source("snapshot MPRIS owners", error))?
    .map_err(|error| TransportError::with_source("snapshot MPRIS owners", error))?;
    shared.ready.store(true, Ordering::Release);
    loop {
        tokio::select! {
            message = playback.next() => {
                let message = message.ok_or(TransportError::ChannelClosed)?.map_err(|error| TransportError::with_source("read MPRIS playback history", error))?;
                let header = message.header();
                let Some(owner) = header.sender().map(|owner| owner.as_str()) else { shared.invalidate(); continue; };
                let Some(bus_names) = names.get(owner) else { shared.invalidate(); continue; };
                let Ok((_, properties, invalidated)) = message.body().deserialize::<(String, HashMap<String, zbus::zvariant::OwnedValue>, Vec<String>)>() else { shared.invalidate(); continue; };
                if invalidated.iter().any(|property| property == "PlaybackStatus") { shared.invalidate(); continue; }
                if let Some(value) = properties.get("PlaybackStatus") {
                    let Ok(status) = <&str>::try_from(value) else { shared.invalidate(); continue; };
                    for bus_name in bus_names {
                        let event = PlayerEvent::Playback { bus_name: bus_name.clone(), owner: owner.to_owned(), playback_status: status.to_owned() };
                        let epoch = shared.epoch.load(Ordering::Acquire);
                        if sender.try_send((epoch, event)).is_err() { shared.invalidate(); break; }
                    }
                }
            },
            message = owners.next() => {
                let message = message.ok_or(TransportError::ChannelClosed)?.map_err(|error| TransportError::with_source("read MPRIS owner history", error))?;
                if let Ok((name, old, new)) = message.body().deserialize::<(String, String, String)>() {
                    if let Some(aliases) = names.get_mut(&old) { aliases.retain(|alias| alias != &name); }
                    if !new.is_empty() { names.entry(new).or_default().push(name); }
                }
                shared.invalidate();
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn ready(monitor: &PlayerMonitor) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !monitor.is_ready() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("history monitor ready");
    }
    async fn emit(connection: &zbus::Connection, status: &str) {
        let mut properties = HashMap::new();
        properties.insert("PlaybackStatus", zbus::zvariant::Value::from(status));
        connection
            .emit_signal(
                None::<&str>,
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(
                    "org.mpris.MediaPlayer2.Player",
                    properties,
                    Vec::<String>::new(),
                ),
            )
            .await
            .expect("playback event");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordered_history_survives_cancelled_wait_and_invalidates_owner_changes() {
        let bus = crate::test_bus::TestBus::start();
        let player = zbus::connection::Builder::address(bus.address.as_str())
            .expect("bus")
            .name("org.mpris.MediaPlayer2.test")
            .expect("player name")
            .build()
            .await
            .expect("player");
        let mut monitor = PlayerMonitor::start_at_bus(bus.address.clone()).expect("monitor");
        ready(&monitor).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), monitor.next_event())
                .await
                .is_err()
        );
        for status in ["Playing", "Paused"] {
            emit(&player, status).await;
        }
        for expected in ["Playing", "Paused"] {
            let event = tokio::time::timeout(Duration::from_secs(1), monitor.next_event())
                .await
                .expect("event deadline")
                .expect("event");
            assert!(
                matches!(event, PlayerEvent::Playback { ref bus_name, ref owner, ref playback_status } if bus_name == "org.mpris.MediaPlayer2.test" && owner == player.unique_name().expect("owner").as_str() && playback_status == expected)
            );
        }
        player
            .release_name("org.mpris.MediaPlayer2.test")
            .await
            .expect("release player name");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), monitor.next_event())
                .await
                .expect("owner invalidation deadline"),
            Some(PlayerEvent::Invalidate { .. })
        ));
        let started = std::time::Instant::now();
        monitor.shutdown();
        assert!(!monitor.is_ready());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_overflow_invalidates_history_before_any_stale_playback() {
        let bus = crate::test_bus::TestBus::start();
        let player = zbus::connection::Builder::address(bus.address.as_str())
            .expect("bus")
            .name("org.mpris.MediaPlayer2.test")
            .expect("name")
            .build()
            .await
            .expect("player");
        let mut monitor = PlayerMonitor::start_at_bus(bus.address.clone()).expect("monitor");
        ready(&monitor).await;
        for _ in 0..CAPACITY + 10 {
            emit(&player, "Paused").await;
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while monitor.shared.epoch.load(Ordering::Acquire) < 10 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("overflow recorded");
        assert!(matches!(
            monitor.try_next_event(),
            Some(PlayerEvent::Invalidate { .. })
        ));
        // Old buffered Paused events cannot create a fresh ownership baseline.
        while let Ok(event) =
            tokio::time::timeout(Duration::from_millis(30), monitor.next_event()).await
        {
            assert!(
                matches!(event, Some(PlayerEvent::Invalidate { .. })),
                "stale playback survived overflow"
            );
        }
        emit(&player, "Playing").await;
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(1), monitor.next_event()).await.expect("fresh event deadline"), Some(PlayerEvent::Playback { playback_status, .. }) if playback_status == "Playing")
        );
        monitor.shutdown();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aliased_player_names_each_receive_the_same_ordered_transition() {
        let bus = crate::test_bus::TestBus::start();
        let player = zbus::connection::Builder::address(bus.address.as_str())
            .expect("bus")
            .name("org.mpris.MediaPlayer2.one")
            .expect("name")
            .build()
            .await
            .expect("player");
        player
            .request_name("org.mpris.MediaPlayer2.two")
            .await
            .expect("second player alias");
        let mut monitor = PlayerMonitor::start_at_bus(bus.address.clone()).expect("monitor");
        ready(&monitor).await;
        assert!(monitor.try_next_event().is_none());
        emit(&player, "Playing").await;
        let mut observed = Vec::new();
        for _ in 0..2 {
            match tokio::time::timeout(Duration::from_secs(1), monitor.next_event())
                .await
                .expect("aliased event deadline")
                .expect("aliased event")
            {
                PlayerEvent::Playback {
                    bus_name,
                    playback_status,
                    ..
                } => {
                    assert_eq!(playback_status, "Playing");
                    observed.push(bus_name);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        observed.sort();
        assert_eq!(
            observed,
            ["org.mpris.MediaPlayer2.one", "org.mpris.MediaPlayer2.two"]
        );
        monitor.shutdown();
    }
}
