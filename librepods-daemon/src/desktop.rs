// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Desktop policy coordination. All effects are bounded, optional and injectable.

use crate::{
    config::{Config, EarPolicy as ConfigEar},
    snapshot::Snapshot,
};
use librepods_core::{
    device::{
        battery::{BatteryStatus, Component},
        state::{ObservationSource, WearReport},
    },
    policy::{
        audio::{
            AudioAction as RecoveryAction, AudioRecoveryPolicy, AudioSnapshot as PolicyAudio,
            CaptureState, DuckingPolicy,
        },
        ear::{EarAction, EarMode, EarPolicy},
        media::{PauseOwnership, PlaybackState, PlayerSnapshot as PolicyPlayer},
        notification::LowBatteryLatch,
    },
    transport::TransportError,
};
use librepods_linux::desktop::{
    self, AudioAction, AudioSnapshot, PlayerAction, PlayerEvent, PlayerMonitor, PlayerSnapshot,
};
use serde_json::{json, Value};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::watch;

type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, TransportError>> + Send + 'a>>;

/// Tests replace this boundary rather than touch the user's live audio session.
pub trait DesktopAdapter: Send + Sync {
    fn player_events_ready(&self) -> bool {
        true
    }
    fn try_player_event(&self) -> Option<PlayerEvent> {
        None
    }
    fn next_player_event(&self) -> Pin<Box<dyn Future<Output = Option<PlayerEvent>> + Send + '_>> {
        Box::pin(std::future::pending())
    }

    fn audio_snapshot<'a>(&'a self, address: &'a str) -> AdapterFuture<'a, AudioSnapshot>;
    fn audio_action<'a>(&'a self, address: &'a str, action: AudioAction) -> AdapterFuture<'a, ()>;
    fn players(&self) -> AdapterFuture<'_, Vec<PlayerSnapshot>>;
    fn player_action<'a>(
        &'a self,
        player: &'a PlayerSnapshot,
        action: PlayerAction,
    ) -> AdapterFuture<'a, PlayerSnapshot>;
    fn notify<'a>(&'a self, summary: &'a str, body: &'a str) -> AdapterFuture<'a, ()>;
}
struct SystemDesktop {
    players: Option<PlayerMonitor>,
}
impl DesktopAdapter for SystemDesktop {
    fn player_events_ready(&self) -> bool {
        self.players.as_ref().is_some_and(PlayerMonitor::is_ready)
    }
    fn try_player_event(&self) -> Option<PlayerEvent> {
        self.players
            .as_ref()
            .and_then(PlayerMonitor::try_next_event)
    }
    fn next_player_event(&self) -> Pin<Box<dyn Future<Output = Option<PlayerEvent>> + Send + '_>> {
        Box::pin(async {
            match &self.players {
                Some(players) => players.next_event().await,
                None => std::future::pending().await,
            }
        })
    }

    fn audio_snapshot<'a>(&'a self, address: &'a str) -> AdapterFuture<'a, AudioSnapshot> {
        Box::pin(desktop::audio_snapshot(address))
    }
    fn audio_action<'a>(&'a self, address: &'a str, action: AudioAction) -> AdapterFuture<'a, ()> {
        Box::pin(desktop::audio_action(address, action))
    }
    fn players(&self) -> AdapterFuture<'_, Vec<PlayerSnapshot>> {
        Box::pin(desktop::players())
    }
    fn player_action<'a>(
        &'a self,
        player: &'a PlayerSnapshot,
        action: PlayerAction,
    ) -> AdapterFuture<'a, PlayerSnapshot> {
        Box::pin(desktop::player_action(player, action))
    }
    fn notify<'a>(&'a self, summary: &'a str, body: &'a str) -> AdapterFuture<'a, ()> {
        Box::pin(desktop::notify(summary, body))
    }
}

pub async fn run(
    config: Config,
    status: watch::Receiver<Snapshot>,
    audio_updates: watch::Sender<Value>,
    manual_overrides: watch::Receiver<u64>,
    shutdown: watch::Receiver<bool>,
) {
    run_with_adapter(
        config,
        status,
        audio_updates,
        manual_overrides,
        shutdown,
        Arc::new(SystemDesktop {
            players: PlayerMonitor::start().ok(),
        }),
    )
    .await;
}

pub async fn run_with_adapter(
    config: Config,
    mut status: watch::Receiver<Snapshot>,
    audio_updates: watch::Sender<Value>,
    mut manual_overrides: watch::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
    adapter: Arc<dyn DesktopAdapter>,
) {
    let mut coordinator = Coordinator::new(config);
    let started = tokio::time::Instant::now();
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut first = true;
    let mut events_open = true;
    loop {
        if *shutdown.borrow() {
            break;
        }
        if !first {
            tokio::select! {
                biased;
                result=shutdown.changed()=>{if result.is_err()||*shutdown.borrow(){break;}},
                result=manual_overrides.changed()=>{if result.is_err(){break;} coordinator.manual_override();},
                event=adapter.next_player_event(), if events_open=>{if event.is_none(){events_open=false;}coordinator.player_event(event);},
                result=status.changed()=>{if result.is_err(){break;}},
                _=interval.tick()=>{},
            }
        }
        first = false;
        coordinator.drain_player_events(adapter.as_ref());
        let snapshot = status.borrow_and_update().clone();
        let now = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut status_changed = false;
        let work = coordinator.step(&snapshot, now, adapter.as_ref());
        let outcome = tokio::select! {
            biased;
            result=shutdown.changed()=>{let _=result;break;},
            result=manual_overrides.changed()=>{if result.is_err(){break;} None},
            result=status.changed()=>{if result.is_err(){break;} status_changed=true; None},
            result=tokio::time::timeout(Duration::from_secs(3),work)=>Some(result),
        };
        match outcome {
            None if status_changed => {
                coordinator.cancel_uncertain_effects();
                first = true;
            }
            None => coordinator.manual_override(),
            Some(Err(_)) => {
                coordinator.cancel_uncertain_effects();
                coordinator.last_error = Some("desktop operation timed out".into());
            }
            Some(Ok(())) => {}
        }
        let value = coordinator.presentation();
        audio_updates.send_if_modified(|old| {
            if *old != value {
                *old = value;
                true
            } else {
                false
            }
        });
    }
    coordinator.shutdown(adapter.as_ref()).await;
}

fn ear_mode(mode: ConfigEar) -> EarMode {
    match mode {
        ConfigEar::Off => EarMode::Off,
        ConfigEar::OneRemoved => EarMode::OneRemoved,
        ConfigEar::BothRemoved => EarMode::BothRemoved,
    }
}
fn players_for_policy(players: &[PlayerSnapshot]) -> Vec<PolicyPlayer> {
    players
        .iter()
        .map(|player| PolicyPlayer {
            id: player.bus_name.clone(),
            owner: player.owner.clone(),
            playback: match player.playback_status.as_str() {
                "Playing" => PlaybackState::Playing,
                "Paused" => PlaybackState::Paused,
                "Stopped" => PlaybackState::Stopped,
                _ => PlaybackState::Unknown,
            },
            can_pause: player.can_pause,
            can_play: player.can_play,
        })
        .collect()
}
fn stamp(snapshot: &Snapshot, field: &str) -> Option<(u64, u64)> {
    snapshot
        .device
        .observations
        .get(field)
        .filter(|meta| {
            !meta.stale
                && meta.generation == snapshot.session_generation
                && meta.source == ObservationSource::Aacp
        })
        .map(|meta| (meta.generation, meta.observed_at_ms))
}
fn policy_audio(audio: &AudioSnapshot) -> PolicyAudio {
    let profile = audio
        .profiles
        .iter()
        .find(|profile| Some(&profile.name) == audio.profile.as_ref());
    let output = audio.outputs.iter().find(|node| node.selected_device);
    let input = audio
        .inputs
        .iter()
        .find(|node| node.selected_device && !node.monitor);
    PolicyAudio {
        server_available: true,
        device_connected: audio.card_name.is_some(),
        profile: audio.profile.clone(),
        profile_available: profile.is_some_and(|p| p.available),
        profile_has_sink: profile.is_some_and(|p| p.sinks > 0),
        profile_has_source: profile.is_some_and(|p| p.sources > 0),
        sink: output.map(|node| node.name.clone()),
        source: input.map(|node| node.name.clone()),
        default_output: audio.default_output.clone(),
        default_input: audio.default_input.clone(),
        muted: output.is_some_and(|node| node.muted),
        volume: output
            .and_then(|node| node.volume_percent)
            .unwrap_or(100.0)
            .clamp(0.0, 100.0)
            .round() as u8,
        capture: if !audio.capture_known {
            CaptureState::Unknown
        } else if audio.capture_active {
            CaptureState::Live
        } else {
            CaptureState::Idle
        },
        pinned_streams: audio.playback_streams.iter().any(|stream| {
            !stream.corked && output.is_some_and(|output| stream.target_index != Some(output.index))
        }),
    }
}

struct RouteOwnership {
    previous: String,
    selected: String,
}
struct Coordinator {
    config: Config,
    address: String,
    generation: u64,
    audio: Option<AudioSnapshot>,
    next_audio_ms: u64,
    last_error: Option<String>,
    last_recovery: Option<String>,
    ear: EarPolicy,
    ear_mode: ConfigEar,
    last_wear: Option<(u64, u64, WearReport)>,
    last_ca: Option<(u64, u64, u8)>,
    next_players_ms: u64,
    media_history_ready: bool,
    ownership: PauseOwnership,
    ducking: DuckingPolicy,
    ducked_volume: Option<(String, u8)>,
    latches: [LowBatteryLatch; 4],
    recovery: AudioRecoveryPolicy,
    route: Option<RouteOwnership>,
    route_attempted: bool,
    yielded: bool,
}
impl Coordinator {
    fn new(config: Config) -> Self {
        Self {
            ear: EarPolicy::new(ear_mode(config.ear_policy), 500),
            ear_mode: config.ear_policy,
            recovery: AudioRecoveryPolicy::new(config.audio_recovery, 500, 6),
            config,
            address: String::new(),
            generation: 0,
            audio: None,
            next_audio_ms: 0,
            last_error: None,
            last_recovery: None,
            last_wear: None,
            last_ca: None,
            next_players_ms: 0,
            media_history_ready: false,
            ownership: PauseOwnership::default(),
            ducking: DuckingPolicy::default(),
            ducked_volume: None,
            latches: std::array::from_fn(|_| LowBatteryLatch::default()),
            route: None,
            route_attempted: false,
            yielded: false,
        }
    }
    fn player_event(&mut self, event: Option<PlayerEvent>) {
        match event {
            Some(PlayerEvent::Playback {
                bus_name,
                owner,
                playback_status,
            }) => {
                let state = match playback_status.as_str() {
                    "Paused" => PlaybackState::Paused,
                    "Playing" => PlaybackState::Playing,
                    "Stopped" => PlaybackState::Stopped,
                    _ => PlaybackState::Unknown,
                };
                self.ownership.observe_playback(&bus_name, &owner, state);
            }
            Some(PlayerEvent::Invalidate { .. }) | None => {
                self.ownership.clear();
                self.ear.reset();
            }
        }
    }
    fn drain_player_events(&mut self, adapter: &dyn DesktopAdapter) {
        // The adapter bounds its queue. Overflow produces invalidation rather than lost history.
        for _ in 0..65 {
            let Some(event) = adapter.try_player_event() else {
                return;
            };
            self.player_event(Some(event));
        }
        self.ownership.clear();
    }
    fn cancel_uncertain_effects(&mut self) {
        self.ownership.clear();
        self.ear.reset();
        self.ducking.cancel();
        self.ducked_volume = None;
    }
    fn manual_override(&mut self) {
        self.cancel_uncertain_effects();
        self.recovery.manual_override();
        self.route = None;
        self.yielded = true;
    }
    fn presentation(&self) -> Value {
        match &self.audio {
            Some(audio) => {
                json!({"health":if self.last_error.is_some(){"degraded".into()}else{format!("{:?}",policy_audio(audio).health())},"server_name":audio.server_name,"profile":audio.profile,"profiles":audio.profiles,"default_output":audio.default_output,"default_input":audio.default_input,"outputs":audio.outputs,"inputs":audio.inputs,"playback_streams":audio.playback_streams,"recording_streams":audio.recording_streams,"capture_active":audio.capture_active,"capture_known":audio.capture_known,"media_history_ready":self.media_history_ready,"routing_policy":if self.config.auto_route{"follow_device"}else{"observe"},"audio_recovery":self.config.audio_recovery&&!self.yielded,"automation_yielded":self.yielded,"last_recovery":self.last_recovery,"last_error":self.last_error})
            }
            None => {
                json!({"health":if self.last_error.is_some(){"unavailable"}else{"not_observed"},"last_error":self.last_error})
            }
        }
    }
    async fn step(&mut self, snapshot: &Snapshot, now: u64, adapter: &dyn DesktopAdapter) {
        self.media_history_ready = adapter.player_events_ready();
        if self.address != snapshot.device.address {
            self.cancel_uncertain_effects();
            self.address = snapshot.device.address.clone();
            self.next_audio_ms = 0;
            self.audio = None;
            self.route = None;
            self.route_attempted = false;
            self.yielded = false;
            self.latches = std::array::from_fn(|_| LowBatteryLatch::default());
            self.recovery = AudioRecoveryPolicy::new(self.config.audio_recovery, 500, 6);
        }
        if self.generation != snapshot.session_generation {
            self.cancel_uncertain_effects();
            self.generation = snapshot.session_generation;
            self.last_wear = None;
            self.last_ca = None;
            self.next_audio_ms = 0;
        }
        if self.ear_mode != snapshot.ear_policy {
            self.ear_mode = snapshot.ear_policy;
            self.ear.set_mode(ear_mode(self.ear_mode));
            self.ownership.clear();
        }
        if self.address.is_empty() {
            return;
        }
        self.notifications(snapshot, adapter).await;
        if now >= self.next_audio_ms {
            self.next_audio_ms = now.saturating_add(
                if self.config.audio_recovery
                    || self.config.auto_route
                    || self.config.ca_ducking
                    || self.ear_mode != ConfigEar::Off
                {
                    1000
                } else {
                    5000
                },
            );
            match adapter.audio_snapshot(&self.address).await {
                Ok(audio) => {
                    self.audio = Some(audio);
                    self.last_error = None;
                }
                Err(error) => {
                    self.cancel_uncertain_effects();
                    self.last_error = Some(format!("audio observation: {}", error.code()));
                    return;
                }
            }
        }
        let Some(audio) = self.audio.clone() else {
            return;
        };
        if self.last_error.is_some() {
            return;
        }
        if self.ownership.has_owned() && now >= self.next_players_ms {
            self.next_players_ms = now.saturating_add(1000);
            match adapter.players().await {
                Ok(players) => self.ownership.observe(&players_for_policy(&players)),
                Err(error) => {
                    self.ownership.clear();
                    self.last_error = Some(format!("MPRIS ownership: {}", error.code()));
                }
            }
        }
        self.routing(snapshot, &audio, adapter).await;
        self.audio_recovery(snapshot, &audio, now, adapter).await;
        self.wear(snapshot, &audio, now, adapter).await;
        self.ca(snapshot, &audio, adapter).await;
    }
    async fn routing(
        &mut self,
        snapshot: &Snapshot,
        audio: &AudioSnapshot,
        adapter: &dyn DesktopAdapter,
    ) {
        if let Some(route) = &self.route {
            if audio.default_output.as_deref() != Some(&route.selected) {
                self.route = None;
                self.yielded = true;
            } else if !audio.outputs.iter().any(|node| node.name == route.selected) {
                if audio.outputs.iter().any(|node| node.name == route.previous) {
                    let action = AudioAction::SetDefaultOutput {
                        name: route.previous.clone(),
                    };
                    if let Err(error) = adapter.audio_action(&self.address, action).await {
                        self.last_error = Some(format!("restore routing: {}", error.code()));
                    }
                }
                self.route = None;
            }
        }
        if !self.config.auto_route || self.yielded || self.route_attempted || !snapshot.connected {
            return;
        }
        let Some(output) = audio.outputs.iter().find(|node| node.selected_device) else {
            return;
        };
        if audio.default_output.as_deref() == Some(&output.name) {
            self.route_attempted = true;
            return;
        }
        if !audio.capture_known || audio.capture_active {
            return;
        }
        self.route_attempted = true;
        let prior = audio.default_output.clone();
        match adapter
            .audio_action(
                &self.address,
                AudioAction::SetDefaultOutput {
                    name: output.name.clone(),
                },
            )
            .await
        {
            Ok(()) => {
                self.route = prior.map(|previous| RouteOwnership {
                    previous,
                    selected: output.name.clone(),
                });
                self.next_audio_ms = 0;
            }
            Err(error) => self.last_error = Some(format!("automatic routing: {}", error.code())),
        }
    }
    async fn audio_recovery(
        &mut self,
        snapshot: &Snapshot,
        audio: &AudioSnapshot,
        now: u64,
        adapter: &dyn DesktopAdapter,
    ) {
        let facts = policy_audio(audio);
        if facts.profile_available && facts.sink.is_some() && !facts.profile_has_source {
            if let Some(profile) = &facts.profile {
                self.recovery.recovered(profile);
            }
        }
        if let RecoveryAction::ActivateProfile(profile) =
            self.recovery
                .observe(&facts, snapshot.session_generation, now)
        {
            match adapter
                .audio_action(&self.address, AudioAction::SetProfile { profile })
                .await
            {
                Ok(()) => {
                    self.last_recovery = Some("profile_activation_requested".into());
                    self.next_audio_ms = 0;
                }
                Err(error) => {
                    self.last_error = Some(format!("audio recovery: {}", error.code()));
                    self.last_recovery = Some("failed".into());
                }
            }
        }
    }
    async fn wear(
        &mut self,
        snapshot: &Snapshot,
        audio: &AudioSnapshot,
        now: u64,
        adapter: &dyn DesktopAdapter,
    ) {
        if !snapshot.connected || self.ear_mode == ConfigEar::Off || !adapter.player_events_ready()
        {
            self.ownership.clear();
            self.ear.reset();
            return;
        }
        let active = audio
            .outputs
            .iter()
            .any(|node| node.selected_device && Some(&node.name) == audio.default_output.as_ref());
        if !active || !audio.capture_known || audio.capture_active {
            self.ownership.clear();
            self.ear.reset();
            return;
        }
        let observed = stamp(snapshot, "wear")
            .zip(snapshot.device.wear)
            .map(|((generation, time), wear)| (generation, time, wear));
        if observed.is_none() {
            self.ownership.clear();
            self.ear.reset();
            return;
        }
        let action = if observed.is_some() && observed != self.last_wear {
            self.last_wear = observed;
            if let Some(wear) = snapshot.device.wear {
                if wear.primary.in_ear().is_none() || wear.secondary.in_ear().is_none() {
                    self.ownership.clear();
                }
                self.ear.on_wear(
                    wear,
                    snapshot.session_generation,
                    now,
                    active,
                    self.ownership.has_owned(),
                )
            } else {
                EarAction::None
            }
        } else {
            self.ear.tick(
                snapshot.session_generation,
                now,
                active,
                self.ownership.has_owned(),
            )
        };
        if action == EarAction::None {
            return;
        }
        // The user may have routed audio elsewhere since the last health refresh.
        let current = match adapter.audio_snapshot(&self.address).await {
            Ok(current) => current,
            Err(error) => {
                self.ownership.clear();
                self.last_error = Some(format!("MPRIS output check: {}", error.code()));
                return;
            }
        };
        if !current.capture_known
            || current.capture_active
            || !current.outputs.iter().any(|node| {
                node.selected_device && Some(&node.name) == current.default_output.as_ref()
            })
        {
            self.ownership.clear();
            self.ear.reset();
            return;
        }
        let mut players = match adapter.players().await {
            Ok(players) => players,
            Err(error) => {
                self.ownership.clear();
                self.last_error = Some(format!("MPRIS observation: {}", error.code()));
                return;
            }
        };
        self.drain_player_events(adapter);
        self.ownership.observe(&players_for_policy(&players));
        let targets = match action {
            EarAction::Pause => self.ownership.request_pause(&players_for_policy(&players)),
            EarAction::Resume => self.ownership.resume_targets(&players_for_policy(&players)),
            EarAction::None => Vec::new(),
        };
        for target in targets {
            let Some(index) = players
                .iter()
                .position(|player| player.bus_name == target.id && player.owner == target.owner)
            else {
                continue;
            };
            let action = if action == EarAction::Pause {
                PlayerAction::Pause
            } else {
                PlayerAction::Play
            };
            match adapter.player_action(&players[index], action).await {
                Ok(observed) => players[index] = observed,
                Err(error) => {
                    self.ownership.pause_failed(&target);
                    self.last_error = Some(format!("MPRIS action: {}", error.code()));
                }
            }
        }
        self.ownership.observe(&players_for_policy(&players));
    }
    async fn ca(
        &mut self,
        snapshot: &Snapshot,
        _audio: &AudioSnapshot,
        adapter: &dyn DesktopAdapter,
    ) {
        if !self.config.ca_ducking || !snapshot.connected {
            return;
        }
        let observed = stamp(snapshot, "ca_event")
            .zip(snapshot.device.ca_event)
            .map(|((generation, time), event)| (generation, time, event));
        if observed.is_none() || observed == self.last_ca {
            return;
        }
        self.last_ca = observed;
        // Refresh before restoration so a cached ducked value cannot overwrite manual volume.
        let current = match adapter.audio_snapshot(&self.address).await {
            Ok(snapshot) => snapshot,
            Err(_) => {
                self.ducking.cancel();
                self.ducked_volume = None;
                return;
            }
        };
        let Some(output) = current.outputs.iter().find(|node| {
            node.selected_device && Some(&node.name) == current.default_output.as_ref()
        }) else {
            self.ducking.cancel();
            self.ducked_volume = None;
            return;
        };
        let Some(current_volume) = output
            .volume_percent
            .filter(|value| value.is_finite() && *value >= 0.0 && *value <= 100.0)
            .map(|value| value.round() as u8)
        else {
            return;
        };
        if let Some(event) = snapshot.device.ca_event {
            if let Some(action) = self.ducking.on_event(event, &output.name, current_volume) {
                let name = action.sink.clone();
                let percent = action.percent;
                match adapter
                    .audio_action(
                        &self.address,
                        AudioAction::SetVolume {
                            name: name.clone(),
                            input: false,
                            percent,
                        },
                    )
                    .await
                {
                    Ok(()) => {
                        self.ducked_volume = if self.ducking.is_ducking() {
                            Some((name, percent))
                        } else {
                            None
                        };
                        self.next_audio_ms = 0;
                    }
                    Err(error) => {
                        self.ducking.cancel();
                        self.ducked_volume = None;
                        self.last_error = Some(format!("CA volume: {}", error.code()));
                    }
                }
            } else if !self.ducking.is_ducking() {
                self.ducked_volume = None;
            }
        }
    }
    async fn notifications(&mut self, snapshot: &Snapshot, adapter: &dyn DesktopAdapter) {
        if !self.config.low_battery_notifications {
            return;
        }
        for (index, component) in [
            Component::Left,
            Component::Right,
            Component::Case,
            Component::Headset,
        ]
        .iter()
        .enumerate()
        {
            let reading = snapshot.device.battery.get(*component);
            let meta = snapshot
                .device
                .observations
                .get(&format!("battery.{}", component.name().to_lowercase()));
            let fresh = meta
                .is_some_and(|meta| !meta.stale && meta.generation == snapshot.session_generation);
            let level = if matches!(
                reading.status,
                BatteryStatus::Disconnected | BatteryStatus::Unknown(_)
            ) {
                None
            } else {
                Some(reading.level)
            };
            let charging = matches!(
                reading.status,
                BatteryStatus::Charging | BatteryStatus::OptimizedCharging
            );
            if let Some(level) = self.latches[index].evaluate(level, charging, fresh) {
                if let Err(error) = adapter
                    .notify(
                        "AirPods battery low",
                        &format!("{} battery: {level}%", component.name()),
                    )
                    .await
                {
                    self.last_error = Some(format!("desktop notification: {}", error.code()));
                }
            }
        }
    }
    async fn shutdown(&mut self, adapter: &dyn DesktopAdapter) {
        if let Some((sink, _)) = &self.ducked_volume {
            if let Ok(Ok(current)) = tokio::time::timeout(
                Duration::from_millis(500),
                adapter.audio_snapshot(&self.address),
            )
            .await
            {
                if let Some(output) = current.outputs.iter().find(|node| {
                    &node.name == sink && Some(&node.name) == current.default_output.as_ref()
                }) {
                    if let Some(volume) = output
                        .volume_percent
                        .filter(|value| value.is_finite() && *value >= 0.0 && *value <= 100.0)
                    {
                        if let Some(action) =
                            self.ducking.speech_stopped(sink, volume.round() as u8)
                        {
                            let _ = tokio::time::timeout(
                                Duration::from_millis(500),
                                adapter.audio_action(
                                    &self.address,
                                    AudioAction::SetVolume {
                                        name: action.sink,
                                        input: false,
                                        percent: action.percent,
                                    },
                                ),
                            )
                            .await;
                        }
                    }
                }
            }
        }
        self.cancel_uncertain_effects();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librepods_core::device::state::{DeviceState, ObservationMeta, WearReport, WearState};
    use librepods_linux::desktop::{AudioNode, AudioProfile};
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
    };

    struct Fake {
        audio: Mutex<AudioSnapshot>,
        players: Mutex<Vec<PlayerSnapshot>>,
        effects: Mutex<Vec<String>>,
        events: Mutex<VecDeque<PlayerEvent>>,
        event_notify: tokio::sync::Notify,
        ready: AtomicBool,
        fail_audio: bool,
        fail_effect: bool,
        stall: bool,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                audio: Mutex::new(AudioSnapshot {
                    card_name: Some("card".into()),
                    default_output: Some("pods".into()),
                    profile: Some("aac".into()),
                    profiles: vec![AudioProfile {
                        name: "aac".into(),
                        available: true,
                        sinks: 1,
                        sources: 0,
                    }],
                    outputs: vec![
                        AudioNode {
                            name: "pods".into(),
                            selected_device: true,
                            volume_percent: Some(50.0),
                            ..AudioNode::default()
                        },
                        AudioNode {
                            name: "speakers".into(),
                            ..AudioNode::default()
                        },
                    ],
                    capture_known: true,
                    ..AudioSnapshot::default()
                }),
                players: Mutex::new(vec![PlayerSnapshot {
                    bus_name: "player".into(),
                    owner: ":1.2".into(),
                    playback_status: "Playing".into(),
                    can_pause: true,
                    can_play: true,
                }]),
                effects: Mutex::new(Vec::new()),
                events: Mutex::new(VecDeque::new()),
                event_notify: tokio::sync::Notify::new(),
                ready: AtomicBool::new(true),
                fail_audio: false,
                fail_effect: false,
                stall: false,
            }
        }
    }
    impl Fake {
        fn emit(&self, event: PlayerEvent) {
            self.events.lock().unwrap().push_back(event);
            self.event_notify.notify_one();
        }
    }
    impl DesktopAdapter for Fake {
        fn player_events_ready(&self) -> bool {
            self.ready.load(Ordering::Acquire)
        }
        fn try_player_event(&self) -> Option<PlayerEvent> {
            self.events.lock().unwrap().pop_front()
        }
        fn next_player_event(
            &self,
        ) -> Pin<Box<dyn Future<Output = Option<PlayerEvent>> + Send + '_>> {
            Box::pin(async {
                loop {
                    let notified = self.event_notify.notified();
                    if let Some(event) = self.try_player_event() {
                        return Some(event);
                    }
                    notified.await;
                }
            })
        }

        fn audio_snapshot<'a>(&'a self, _: &'a str) -> AdapterFuture<'a, AudioSnapshot> {
            Box::pin(async move {
                if self.stall {
                    std::future::pending::<()>().await;
                }
                if self.fail_audio {
                    Err(TransportError::Timeout)
                } else {
                    Ok(self.audio.lock().unwrap().clone())
                }
            })
        }
        fn audio_action<'a>(&'a self, _: &'a str, action: AudioAction) -> AdapterFuture<'a, ()> {
            Box::pin(async move {
                self.effects.lock().unwrap().push(format!("{action:?}"));
                if self.fail_effect {
                    return Err(TransportError::PermissionDenied);
                }
                let mut audio = self.audio.lock().unwrap();
                match action {
                    AudioAction::SetDefaultOutput { name } => audio.default_output = Some(name),
                    AudioAction::SetProfile { profile } => audio.profile = Some(profile),
                    AudioAction::SetVolume { name, percent, .. } => {
                        audio
                            .outputs
                            .iter_mut()
                            .find(|node| node.name == name)
                            .unwrap()
                            .volume_percent = Some(f64::from(percent))
                    }
                    _ => panic!("unexpected action"),
                }
                Ok(())
            })
        }
        fn players(&self) -> AdapterFuture<'_, Vec<PlayerSnapshot>> {
            Box::pin(async { Ok(self.players.lock().unwrap().clone()) })
        }
        fn player_action<'a>(
            &'a self,
            player: &'a PlayerSnapshot,
            action: PlayerAction,
        ) -> AdapterFuture<'a, PlayerSnapshot> {
            Box::pin(async move {
                self.effects.lock().unwrap().push(format!("{action:?}"));
                if self.fail_effect {
                    return Err(TransportError::Timeout);
                }
                let mut players = self.players.lock().unwrap();
                let current = players
                    .iter_mut()
                    .find(|p| p.owner == player.owner && p.bus_name == player.bus_name)
                    .ok_or(TransportError::Cancelled)?;
                current.playback_status = match action {
                    PlayerAction::Pause => "Paused",
                    PlayerAction::Play => "Playing",
                }
                .into();
                Ok(current.clone())
            })
        }
        fn notify<'a>(&'a self, _: &'a str, body: &'a str) -> AdapterFuture<'a, ()> {
            Box::pin(async move {
                self.effects.lock().unwrap().push(body.into());
                if self.fail_effect {
                    Err(TransportError::Timeout)
                } else {
                    Ok(())
                }
            })
        }
    }
    fn snapshot(mode: ConfigEar) -> Snapshot {
        let mut snapshot = Snapshot::new("test".into(), &DeviceState::default());
        snapshot.connected = true;
        snapshot.session_generation = 1;
        snapshot.device.address = "AA:BB:CC:DD:EE:FF".into();
        snapshot.ear_policy = mode;
        snapshot
    }
    fn observe(snapshot: &mut Snapshot, field: &str, time: u64) {
        snapshot.device.observations.insert(
            field.into(),
            ObservationMeta {
                generation: snapshot.session_generation,
                observed_at_ms: time,
                source: ObservationSource::Aacp,
                stale: false,
            },
        );
    }
    fn wear(snapshot: &mut Snapshot, primary: WearState, secondary: WearState, time: u64) {
        snapshot.device.wear = Some(WearReport { primary, secondary });
        observe(snapshot, "wear", time);
    }
    #[tokio::test]
    async fn default_observation_never_mutates_and_notifications_survive_audio_failure() {
        let fake = Fake::default();
        let mut c = Coordinator::new(Config::default());
        let mut s = snapshot(ConfigEar::Off);
        c.step(&s, 0, &fake).await;
        assert!(fake.effects.lock().unwrap().is_empty());
        assert_eq!(c.presentation()["health"], "Healthy");
        let fake = Fake {
            fail_audio: true,
            ..Fake::default()
        };
        c = Coordinator::new(Config {
            low_battery_notifications: true,
            ..Config::default()
        });
        s.device.battery.left.level = 8;
        s.device.battery.left.status = BatteryStatus::Discharging;
        observe(&mut s, "battery.left", 1);
        c.step(&s, 0, &fake).await;
        c.step(&s, 5000, &fake).await;
        assert_eq!(fake.effects.lock().unwrap().len(), 1);
        assert_eq!(c.presentation()["health"], "unavailable");
        s.device.battery.left.level = 20;
        c.step(&s, 10000, &fake).await;
        s.device.battery.left.level = 9;
        c.step(&s, 15000, &fake).await;
        assert_eq!(fake.effects.lock().unwrap().len(), 2);
    }
    #[tokio::test]
    async fn ear_debounce_owns_exact_confirmed_pause_and_rejects_replaced_player() {
        let fake = Fake::default();
        let mut c = Coordinator::new(Config::default());
        let mut s = snapshot(ConfigEar::BothRemoved);
        wear(&mut s, WearState::InEar, WearState::InEar, 0);
        c.step(&s, 0, &fake).await;
        wear(&mut s, WearState::OutOfEar, WearState::OutOfEar, 1);
        c.step(&s, 1, &fake).await;
        c.step(&s, 500, &fake).await;
        assert!(fake.effects.lock().unwrap().is_empty());
        c.step(&s, 501, &fake).await;
        assert_eq!(*fake.effects.lock().unwrap(), vec!["Pause"]);
        wear(&mut s, WearState::InEar, WearState::OutOfEar, 502);
        c.step(&s, 502, &fake).await;
        assert_eq!(*fake.effects.lock().unwrap(), vec!["Pause", "Play"]);
        wear(&mut s, WearState::OutOfEar, WearState::OutOfEar, 503);
        c.step(&s, 503, &fake).await;
        c.step(&s, 1003, &fake).await;
        fake.players.lock().unwrap()[0].owner = ":1.3".into();
        wear(&mut s, WearState::InEar, WearState::InEar, 1004);
        c.step(&s, 1004, &fake).await;
        assert_eq!(
            *fake.effects.lock().unwrap(),
            vec!["Pause", "Play", "Pause"]
        );
        s.connected = false;
        c.step(&s, 2000, &fake).await;
        assert!(!c.ownership.has_owned());
    }
    #[tokio::test]
    async fn stale_wear_unknown_wear_wrong_output_and_failed_pause_never_resume() {
        let fake = Fake {
            fail_effect: true,
            ..Fake::default()
        };
        let mut c = Coordinator::new(Config::default());
        let mut s = snapshot(ConfigEar::OneRemoved);
        wear(&mut s, WearState::InEar, WearState::InEar, 0);
        c.step(&s, 0, &fake).await;
        wear(&mut s, WearState::OutOfEar, WearState::InEar, 1);
        c.step(&s, 1, &fake).await;
        assert!(c.last_error.is_some());
        assert!(!c.ownership.has_owned());
        wear(&mut s, WearState::InEar, WearState::InEar, 2);
        c.step(&s, 1001, &fake).await;
        assert_eq!(fake.effects.lock().unwrap().len(), 1);
        wear(&mut s, WearState::Unknown(3), WearState::InEar, 3);
        c.step(&s, 1002, &fake).await;
        fake.audio.lock().unwrap().default_output = Some("speakers".into());
        c.step(&s, 2002, &fake).await;
        assert!(!c.ownership.has_owned());
        s.device.observations.get_mut("wear").unwrap().stale = true;
        c.step(&s, 2003, &fake).await;
        assert_eq!(fake.effects.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn ca_restores_owned_volume_but_preserves_manual_adjustments_and_shutdown() {
        let fake = Fake::default();
        let mut c = Coordinator::new(Config {
            ca_ducking: true,
            ..Config::default()
        });
        let mut s = snapshot(ConfigEar::Off);
        s.device.ca_event = Some(1);
        observe(&mut s, "ca_event", 0);
        c.step(&s, 0, &fake).await;
        assert_eq!(
            fake.audio.lock().unwrap().outputs[0].volume_percent,
            Some(10.0)
        );
        s.device.ca_event = Some(8);
        observe(&mut s, "ca_event", 1);
        c.step(&s, 1, &fake).await;
        assert_eq!(
            fake.audio.lock().unwrap().outputs[0].volume_percent,
            Some(50.0)
        );
        s.device.ca_event = Some(2);
        observe(&mut s, "ca_event", 2);
        c.step(&s, 2, &fake).await;
        fake.audio.lock().unwrap().outputs[0].volume_percent = Some(35.0);
        s.device.ca_event = Some(9);
        observe(&mut s, "ca_event", 3);
        c.step(&s, 3, &fake).await;
        assert_eq!(
            fake.audio.lock().unwrap().outputs[0].volume_percent,
            Some(35.0)
        );
        s.device.ca_event = Some(1);
        observe(&mut s, "ca_event", 4);
        c.step(&s, 4, &fake).await;
        c.shutdown(&fake).await;
        assert_eq!(
            fake.audio.lock().unwrap().outputs[0].volume_percent,
            Some(35.0)
        );
    }
    #[tokio::test]
    async fn routing_restores_previous_default_and_yields_to_manual_routing() {
        let fake = Fake::default();
        fake.audio.lock().unwrap().default_output = Some("speakers".into());
        let mut c = Coordinator::new(Config {
            auto_route: true,
            ..Config::default()
        });
        let mut s = snapshot(ConfigEar::Off);
        c.step(&s, 0, &fake).await;
        assert_eq!(
            fake.audio.lock().unwrap().default_output.as_deref(),
            Some("pods")
        );
        fake.audio.lock().unwrap().outputs.remove(0);
        s.connected = false;
        c.step(&s, 1000, &fake).await;
        assert_eq!(
            fake.audio.lock().unwrap().default_output.as_deref(),
            Some("speakers")
        );
        let fake = Fake::default();
        fake.audio.lock().unwrap().default_output = Some("speakers".into());
        let mut c = Coordinator::new(Config {
            auto_route: true,
            ..Config::default()
        });
        s.connected = true;
        c.step(&s, 0, &fake).await;
        fake.audio.lock().unwrap().default_output = Some("speakers".into());
        c.step(&s, 1000, &fake).await;
        assert!(c.yielded);
        assert!(c.route.is_none());
        c.manual_override();
        assert!(c.presentation()["automation_yielded"].as_bool().unwrap());
    }
    #[tokio::test]
    async fn recovery_uses_remembered_playback_profile_and_never_interrupts_capture() {
        let fake = Fake::default();
        let mut c = Coordinator::new(Config {
            audio_recovery: true,
            ..Config::default()
        });
        let s = snapshot(ConfigEar::Off);
        c.step(&s, 0, &fake).await;
        {
            let mut audio = fake.audio.lock().unwrap();
            audio.profile = Some("off".into());
            audio.capture_active = true;
        }
        c.step(&s, 1000, &fake).await;
        c.step(&s, 2000, &fake).await;
        assert!(fake.effects.lock().unwrap().is_empty());
        fake.audio.lock().unwrap().capture_active = false;
        c.step(&s, 3000, &fake).await;
        c.step(&s, 3500, &fake).await;
        assert_eq!(fake.audio.lock().unwrap().profile.as_deref(), Some("aac"));
        assert_eq!(
            c.last_recovery.as_deref(),
            Some("profile_activation_requested")
        );
    }
    #[tokio::test]
    async fn ownership_is_invalidated_by_manual_playback_between_ear_reports() {
        let fake = Fake::default();
        let mut c = Coordinator::new(Config::default());
        let mut s = snapshot(ConfigEar::OneRemoved);
        wear(&mut s, WearState::InEar, WearState::InEar, 0);
        c.step(&s, 0, &fake).await;
        wear(&mut s, WearState::OutOfEar, WearState::InEar, 1);
        c.step(&s, 1, &fake).await;
        fake.players.lock().unwrap()[0].playback_status = "Playing".into();
        c.step(&s, 1001, &fake).await;
        fake.players.lock().unwrap()[0].playback_status = "Paused".into();
        wear(&mut s, WearState::InEar, WearState::InEar, 1002);
        c.step(&s, 1002, &fake).await;
        assert_eq!(*fake.effects.lock().unwrap(), vec!["Pause"]);
    }
    #[tokio::test]
    async fn capture_live_or_unknown_blocks_ear_and_route_actions() {
        for known in [false, true] {
            let fake = Fake::default();
            {
                let mut audio = fake.audio.lock().unwrap();
                audio.capture_known = known;
                audio.capture_active = known;
            }
            let mut c = Coordinator::new(Config::default());
            let mut s = snapshot(ConfigEar::OneRemoved);
            wear(&mut s, WearState::InEar, WearState::InEar, 0);
            c.step(&s, 0, &fake).await;
            wear(&mut s, WearState::OutOfEar, WearState::InEar, 1);
            c.step(&s, 1, &fake).await;
            fake.audio.lock().unwrap().default_output = Some("speakers".into());
            let mut c = Coordinator::new(Config {
                auto_route: true,
                ..Config::default()
            });
            c.step(&s, 0, &fake).await;
            assert!(fake.effects.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn failed_volume_route_and_recovery_effects_only_degrade_audio() {
        let fake = Fake {
            fail_effect: true,
            ..Fake::default()
        };
        let mut c = Coordinator::new(Config {
            ca_ducking: true,
            ..Config::default()
        });
        let mut s = snapshot(ConfigEar::Off);
        s.device.ca_event = Some(1);
        observe(&mut s, "ca_event", 0);
        c.step(&s, 0, &fake).await;
        assert!(c.last_error.as_ref().unwrap().starts_with("CA volume:"));
        assert!(c.ducked_volume.is_none());
        assert!(!c.ducking.is_ducking());
        fake.audio.lock().unwrap().default_output = Some("speakers".into());
        let mut c = Coordinator::new(Config {
            auto_route: true,
            ..Config::default()
        });
        c.step(&s, 0, &fake).await;
        assert!(c
            .last_error
            .as_ref()
            .unwrap()
            .starts_with("automatic routing:"));
        assert!(c.route.is_none());
        fake.audio.lock().unwrap().default_output = Some("pods".into());
        let mut c = Coordinator::new(Config {
            audio_recovery: true,
            ..Config::default()
        });
        c.step(&s, 0, &fake).await;
        fake.audio.lock().unwrap().profile = Some("off".into());
        c.step(&s, 1000, &fake).await;
        c.step(&s, 1500, &fake).await;
        assert_eq!(c.last_recovery.as_deref(), Some("failed"));
        assert!(s.connected);
    }
    #[tokio::test]
    async fn invalid_volume_unknown_ca_and_changed_output_never_overwrite_user() {
        let fake = Fake::default();
        let mut c = Coordinator::new(Config {
            ca_ducking: true,
            ..Config::default()
        });
        let mut s = snapshot(ConfigEar::Off);
        s.device.ca_event = Some(255);
        observe(&mut s, "ca_event", 0);
        c.step(&s, 0, &fake).await;
        fake.audio.lock().unwrap().outputs[0].volume_percent = None;
        s.device.ca_event = Some(1);
        observe(&mut s, "ca_event", 1);
        c.step(&s, 1, &fake).await;
        assert!(fake.effects.lock().unwrap().is_empty());
        fake.audio.lock().unwrap().outputs[0].volume_percent = Some(50.0);
        observe(&mut s, "ca_event", 2);
        c.step(&s, 2, &fake).await;
        fake.audio.lock().unwrap().default_output = Some("speakers".into());
        s.device.ca_event = Some(8);
        observe(&mut s, "ca_event", 3);
        c.step(&s, 3, &fake).await;
        assert_eq!(fake.effects.lock().unwrap().len(), 1);
        assert!(!c.ducking.is_ducking());
        c.shutdown(&fake).await;
        assert_eq!(fake.effects.lock().unwrap().len(), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn coordinator_loop_publishes_changes_and_yields_to_manual_override() {
        let (status_sender, status) = watch::channel(snapshot(ConfigEar::Off));
        let (audio, mut audio_view) = watch::channel(Value::Null);
        let (override_sender, overrides) = watch::channel(0);
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run_with_adapter(
            Config::default(),
            status,
            audio,
            overrides,
            shutdown,
            Arc::new(Fake::default()),
        ));
        audio_view.changed().await.unwrap();
        assert_eq!(audio_view.borrow()["health"], "Healthy");
        override_sender.send(1).unwrap();
        audio_view.changed().await.unwrap();
        assert_eq!(audio_view.borrow()["automation_yielded"], true);
        status_sender.send_modify(|snapshot| {
            snapshot.session_generation = 2;
            snapshot.device.address = "".into();
        });
        tokio::task::yield_now().await;
        stop.send(true).unwrap();
        task.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn coordinator_loop_bounds_stalled_effects_and_survives_status_replacement() {
        let (status_sender, status) = watch::channel(snapshot(ConfigEar::Off));
        let (audio, mut audio_view) = watch::channel(Value::Null);
        let (_override_sender, overrides) = watch::channel(0);
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run_with_adapter(
            Config::default(),
            status,
            audio,
            overrides,
            shutdown,
            Arc::new(Fake {
                stall: true,
                ..Fake::default()
            }),
        ));
        tokio::task::yield_now().await;
        status_sender.send_modify(|snapshot| snapshot.session_generation = 2);
        tokio::task::yield_now().await;
        audio_view.changed().await.unwrap();
        assert_eq!(audio_view.borrow_and_update()["health"], "not_observed");
        tokio::time::advance(Duration::from_secs(4)).await;
        audio_view.changed().await.unwrap();
        assert_eq!(
            audio_view.borrow()["last_error"],
            "desktop operation timed out"
        );
        stop.send(true).unwrap();
        task.await.unwrap();
    }
    #[tokio::test]
    async fn queued_manual_play_then_pause_invalidates_ownership_before_resume() {
        for invalidate in [false, true] {
            let fake = Fake::default();
            let mut c = Coordinator::new(Config::default());
            let mut s = snapshot(ConfigEar::OneRemoved);
            wear(&mut s, WearState::InEar, WearState::InEar, 0);
            c.step(&s, 0, &fake).await;
            wear(&mut s, WearState::OutOfEar, WearState::InEar, 1);
            c.step(&s, 1, &fake).await;
            if invalidate {
                fake.emit(PlayerEvent::Invalidate {
                    reason: "user bus lost",
                });
            } else {
                for status in ["Playing", "Paused"] {
                    fake.emit(PlayerEvent::Playback {
                        bus_name: "player".into(),
                        owner: ":1.2".into(),
                        playback_status: status.into(),
                    });
                }
            }
            wear(&mut s, WearState::InEar, WearState::InEar, 2);
            c.step(&s, 2, &fake).await;
            assert_eq!(*fake.effects.lock().unwrap(), vec!["Pause"]);
        }
    }
    #[tokio::test]
    async fn unavailable_playback_history_suppresses_automation_until_a_new_baseline() {
        let fake = Fake::default();
        fake.ready.store(false, Ordering::Release);
        let mut c = Coordinator::new(Config::default());
        let mut s = snapshot(ConfigEar::OneRemoved);
        wear(&mut s, WearState::InEar, WearState::InEar, 0);
        c.step(&s, 0, &fake).await;
        wear(&mut s, WearState::OutOfEar, WearState::InEar, 1);
        c.step(&s, 1, &fake).await;
        fake.ready.store(true, Ordering::Release);
        wear(&mut s, WearState::InEar, WearState::InEar, 2);
        c.step(&s, 2, &fake).await;
        assert!(fake.effects.lock().unwrap().is_empty());
        wear(&mut s, WearState::OutOfEar, WearState::InEar, 3);
        c.step(&s, 3, &fake).await;
        assert_eq!(*fake.effects.lock().unwrap(), vec!["Pause"]);
    }
    #[tokio::test(start_paused = true)]
    async fn loop_processes_ordered_history_before_simultaneous_ear_insert() {
        let fake = Arc::new(Fake::default());
        let mut s = snapshot(ConfigEar::OneRemoved);
        wear(&mut s, WearState::InEar, WearState::InEar, 0);
        let (status_sender, status) = watch::channel(s.clone());
        let (audio, mut view) = watch::channel(Value::Null);
        let (_overrides, overrides) = watch::channel(0);
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run_with_adapter(
            Config::default(),
            status,
            audio,
            overrides,
            shutdown,
            fake.clone(),
        ));
        view.changed().await.unwrap();
        wear(&mut s, WearState::OutOfEar, WearState::InEar, 1);
        status_sender.send(s.clone()).unwrap();
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(*fake.effects.lock().unwrap(), vec!["Pause"]);
        for status in ["Playing", "Paused"] {
            fake.emit(PlayerEvent::Playback {
                bus_name: "player".into(),
                owner: ":1.2".into(),
                playback_status: status.into(),
            });
        }
        wear(&mut s, WearState::InEar, WearState::InEar, 2);
        status_sender.send(s).unwrap();
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        stop.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(*fake.effects.lock().unwrap(), vec!["Pause"]);
    }
    #[tokio::test]
    async fn distinct_reports_with_the_same_millisecond_are_not_dropped() {
        let fake = Fake::default();
        let mut c = Coordinator::new(Config {
            ca_ducking: true,
            ..Config::default()
        });
        let mut s = snapshot(ConfigEar::OneRemoved);
        wear(&mut s, WearState::InEar, WearState::InEar, 10);
        s.device.ca_event = Some(1);
        observe(&mut s, "ca_event", 10);
        c.step(&s, 10, &fake).await;
        wear(&mut s, WearState::OutOfEar, WearState::InEar, 10);
        s.device.ca_event = Some(8);
        c.step(&s, 11, &fake).await;
        assert_eq!(
            fake.audio.lock().unwrap().outputs[0].volume_percent,
            Some(50.0)
        );
        assert!(fake
            .effects
            .lock()
            .unwrap()
            .iter()
            .any(|effect| effect == "Pause"));
    }
    #[tokio::test]
    async fn shutdown_cancels_stalled_observation_without_waiting_for_deadline() {
        let (_status_sender, status) = watch::channel(snapshot(ConfigEar::Off));
        let (audio, _) = watch::channel(Value::Null);
        let (_override_sender, overrides) = watch::channel(0);
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run_with_adapter(
            Config::default(),
            status,
            audio,
            overrides,
            shutdown,
            Arc::new(Fake {
                stall: true,
                ..Fake::default()
            }),
        ));
        tokio::task::yield_now().await;
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .unwrap()
            .unwrap();
    }
}
