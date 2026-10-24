// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Audio health, bounded targeted recovery and volume ownership decisions.

use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureState {
    Idle,
    Live,
    #[default]
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioHealth {
    Healthy,
    ServerUnavailable,
    DeviceDisconnected,
    ProfileOff,
    NoPlaybackProfile,
    MissingSink,
    MissingSource,
    Muted,
    ZeroVolume,
    WrongDefault,
    PinnedStreams,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioSnapshot {
    pub server_available: bool,
    pub device_connected: bool,
    pub profile: Option<String>,
    pub profile_available: bool,
    pub profile_has_sink: bool,
    pub profile_has_source: bool,
    pub sink: Option<String>,
    pub source: Option<String>,
    pub default_output: Option<String>,
    pub default_input: Option<String>,
    pub muted: bool,
    pub volume: u8,
    pub capture: CaptureState,
    pub pinned_streams: bool,
}
impl AudioSnapshot {
    pub fn health(&self) -> AudioHealth {
        if !self.server_available {
            AudioHealth::ServerUnavailable
        } else if !self.device_connected {
            AudioHealth::DeviceDisconnected
        } else if self.profile.as_deref().is_none_or(|p| p == "off") {
            AudioHealth::ProfileOff
        } else if !self.profile_available || !self.profile_has_sink {
            AudioHealth::NoPlaybackProfile
        } else if self.sink.is_none() {
            AudioHealth::MissingSink
        } else if self.profile_has_source && self.source.is_none() {
            AudioHealth::MissingSource
        } else if self.muted {
            AudioHealth::Muted
        } else if self.volume == 0 {
            AudioHealth::ZeroVolume
        } else if self.sink != self.default_output {
            AudioHealth::WrongDefault
        } else if self.pinned_streams {
            AudioHealth::PinnedStreams
        } else {
            AudioHealth::Healthy
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioAction {
    None,
    WaitUntil(u64),
    ActivateProfile(String),
    Suppressed(CaptureState),
    Exhausted,
}
pub struct AudioRecoveryPolicy {
    enabled: bool,
    settlement_ms: u64,
    max_attempts: u32,
    attempts: u32,
    expected_profile: Option<String>,
    pending: Option<(u64, u64)>,
}
impl AudioRecoveryPolicy {
    pub fn new(enabled: bool, settlement_ms: u64, max_attempts: u32) -> Self {
        Self {
            enabled,
            settlement_ms,
            max_attempts,
            attempts: 0,
            expected_profile: None,
            pending: None,
        }
    }
    pub fn remember_profile(&mut self, profile: &str) {
        if profile != "off" {
            self.expected_profile = Some(profile.into());
        }
    }
    pub fn recovered(&mut self, profile: &str) {
        self.remember_profile(profile);
        self.attempts = 0;
        self.pending = None;
    }
    pub fn manual_override(&mut self) {
        self.enabled = false;
        self.pending = None;
    }
    pub fn observe(
        &mut self,
        snapshot: &AudioSnapshot,
        generation: u64,
        now_ms: u64,
    ) -> AudioAction {
        if !self.enabled || !snapshot.server_available || !snapshot.device_connected {
            self.pending = None;
            return AudioAction::None;
        }
        if !matches!(
            snapshot.health(),
            AudioHealth::ProfileOff | AudioHealth::MissingSink | AudioHealth::NoPlaybackProfile
        ) {
            self.pending = None;
            return AudioAction::None;
        }
        if snapshot.capture != CaptureState::Idle {
            return AudioAction::Suppressed(snapshot.capture);
        }
        let Some(profile) = self.expected_profile.clone() else {
            return AudioAction::None;
        };
        if self.attempts >= self.max_attempts {
            return AudioAction::Exhausted;
        }
        let deadline = match self.pending {
            Some((active, deadline)) if active == generation => deadline,
            _ => {
                let deadline = now_ms.saturating_add(self.settlement_ms);
                self.pending = Some((generation, deadline));
                deadline
            }
        };
        if now_ms < deadline {
            return AudioAction::WaitUntil(deadline);
        }
        self.attempts += 1;
        let backoff = 1000_u64.saturating_mul(1 << self.attempts.saturating_sub(1).min(4));
        self.pending = Some((generation, now_ms.saturating_add(backoff)));
        AudioAction::ActivateProfile(profile)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeAction {
    pub sink: String,
    pub percent: u8,
}
#[derive(Default)]
pub struct DuckingPolicy {
    owned: Option<(String, u8, u8)>,
}
impl DuckingPolicy {
    pub fn speech_started(&mut self, sink: &str, volume: u8) -> Option<VolumeAction> {
        if self.owned.is_some() || volume > 100 {
            return None;
        }
        let target = volume / 5;
        self.owned = Some((sink.into(), volume, target));
        Some(VolumeAction {
            sink: sink.into(),
            percent: target,
        })
    }
    pub fn speech_stopped(
        &mut self,
        current_sink: &str,
        current_volume: u8,
    ) -> Option<VolumeAction> {
        let (sink, original, ducked) = self.owned.take()?;
        if sink != current_sink || ducked != current_volume {
            return None;
        }
        Some(VolumeAction {
            sink,
            percent: original,
        })
    }
    pub fn cancel(&mut self) {
        self.owned = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn healthy() -> AudioSnapshot {
        AudioSnapshot {
            server_available: true,
            device_connected: true,
            profile: Some("aac".into()),
            profile_available: true,
            profile_has_sink: true,
            sink: Some("pods".into()),
            default_output: Some("pods".into()),
            volume: 40,
            capture: CaptureState::Idle,
            ..AudioSnapshot::default()
        }
    }
    #[test]
    fn missing_capture_in_a_playback_only_profile_is_not_a_failure() {
        let mut state = healthy();
        assert_eq!(state.health(), AudioHealth::Healthy);
        state.profile_has_source = true;
        assert_eq!(state.health(), AudioHealth::MissingSource);
        state.source = Some("mic".into());
        state.muted = true;
        assert_eq!(state.health(), AudioHealth::Muted);
        state.muted = false;
        state.volume = 0;
        assert_eq!(state.health(), AudioHealth::ZeroVolume);
        state.volume = 40;
        state.default_output = Some("speaker".into());
        assert_eq!(state.health(), AudioHealth::WrongDefault);
        state.default_output = Some("pods".into());
        state.pinned_streams = true;
        assert_eq!(state.health(), AudioHealth::PinnedStreams);
        state.sink = None;
        assert_eq!(state.health(), AudioHealth::MissingSink);
        state.profile_has_sink = false;
        assert_eq!(state.health(), AudioHealth::NoPlaybackProfile);
        state.profile = Some("off".into());
        assert_eq!(state.health(), AudioHealth::ProfileOff);
        state.device_connected = false;
        assert_eq!(state.health(), AudioHealth::DeviceDisconnected);
        state.server_available = false;
        assert_eq!(state.health(), AudioHealth::ServerUnavailable);
    }
    #[test]
    fn recovery_is_opt_in_capture_safe_bounded_and_superseded_by_manual_choices() {
        let mut snapshot = healthy();
        snapshot.profile = Some("off".into());
        let mut policy = AudioRecoveryPolicy::new(false, 500, 2);
        policy.remember_profile("aac");
        assert_eq!(policy.observe(&snapshot, 1, 0), AudioAction::None);
        let mut policy = AudioRecoveryPolicy::new(true, 500, 2);
        policy.remember_profile("aac");
        snapshot.capture = CaptureState::Unknown;
        assert_eq!(
            policy.observe(&snapshot, 1, 0),
            AudioAction::Suppressed(CaptureState::Unknown)
        );
        snapshot.capture = CaptureState::Live;
        assert_eq!(
            policy.observe(&snapshot, 1, 1),
            AudioAction::Suppressed(CaptureState::Live)
        );
        snapshot.capture = CaptureState::Idle;
        assert_eq!(policy.observe(&snapshot, 1, 2), AudioAction::WaitUntil(502));
        assert_eq!(
            policy.observe(&snapshot, 1, 501),
            AudioAction::WaitUntil(502)
        );
        assert_eq!(
            policy.observe(&snapshot, 1, 502),
            AudioAction::ActivateProfile("aac".into())
        );
        assert_eq!(
            policy.observe(&snapshot, 1, 1502),
            AudioAction::ActivateProfile("aac".into())
        );
        assert_eq!(policy.observe(&snapshot, 1, 3502), AudioAction::Exhausted);
        policy.recovered("aac");
        assert_eq!(policy.observe(&healthy(), 1, 3503), AudioAction::None);
        assert_eq!(
            policy.observe(&snapshot, 1, 3504),
            AudioAction::WaitUntil(4004)
        );
        assert_eq!(
            policy.observe(&snapshot, 2, 4004),
            AudioAction::WaitUntil(4504)
        );
        policy.manual_override();
        assert_eq!(policy.observe(&snapshot, 2, 9000), AudioAction::None);
    }
    #[test]
    fn ducking_restores_only_its_own_volume_on_the_same_sink() {
        let mut policy = DuckingPolicy::default();
        assert_eq!(
            policy.speech_started("pods", 50),
            Some(VolumeAction {
                sink: "pods".into(),
                percent: 10
            })
        );
        assert_eq!(policy.speech_started("pods", 10), None);
        assert_eq!(
            policy.speech_stopped("pods", 10),
            Some(VolumeAction {
                sink: "pods".into(),
                percent: 50
            })
        );
        policy.speech_started("pods", 50);
        assert_eq!(policy.speech_stopped("speakers", 10), None);
        policy.speech_started("pods", 50);
        assert_eq!(policy.speech_stopped("pods", 25), None);
        assert_eq!(policy.speech_started("pods", 101), None);
        policy.speech_started("pods", 50);
        policy.cancel();
        assert_eq!(policy.speech_stopped("pods", 10), None);
    }
}
