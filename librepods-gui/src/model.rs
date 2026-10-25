// SPDX-License-Identifier: AGPL-3.0-only
use librepods_api::{
    config::EarPolicy,
    ipc::Operation,
    snapshot::{available, Snapshot},
};
use librepods_core::{
    device::{
        battery::{BatteryState, BatteryStatus},
        state::NoiseControlMode,
    },
    protocol::settings::SettingChange,
};
use serde_json::Value;

/// Polls expire independently of retained device observations.
pub const STATUS_TTL_MS: u64 = 3500;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub instance: String,
    pub address: String,
    pub generation: u64,
}
impl Target {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        Self {
            instance: s.daemon_instance.clone(),
            address: s.device.address.clone(),
            generation: s.session_generation,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Mode(NoiseControlMode),
    Conversation(bool),
    OneBud(bool),
    Adaptive(u8),
    EarPolicy(EarPolicy),
    Connect,
    Disconnect,
    Forget,
}
#[derive(Debug, Clone)]
pub struct Command {
    pub target: Target,
    pub action: Action,
}
impl Command {
    pub fn operation(&self) -> Operation {
        let op = match self.action {
            Action::Mode(v) => Operation::Change(SettingChange::ListeningMode(v)),
            Action::Conversation(v) => Operation::Change(SettingChange::ConversationalAwareness(v)),
            Action::OneBud(v) => Operation::Change(SettingChange::SingleEarbudAnc(v)),
            Action::Adaptive(v) => Operation::Change(SettingChange::AdaptiveNoiseLevel(v)),
            Action::EarPolicy(v) => Operation::EarPolicy(v),
            Action::Connect => Operation::DeviceConnect,
            Action::Disconnect => Operation::DeviceDisconnect,
            Action::Forget => Operation::DeviceForget { confirmed: true },
        };
        Operation::ForDevice {
            address: self.target.address.clone(),
            operation: Box::new(op),
        }
    }
    /// Rechecked by the worker against a new status before sending any mutation.
    pub fn validate(&self, s: &Snapshot, caps: &Value) -> Result<(), String> {
        if self.target != Target::from_snapshot(s) {
            return Err(
                "Device or session changed. Review the current device and try again.".into(),
            );
        }
        validate_action(s, caps, self.action)
    }
}

#[derive(Default)]
pub struct PanelModel {
    pub snapshot: Option<Snapshot>,
    pub capabilities: Value,
    pub reachable: bool,
    pub pending: bool,
    pub message: Option<String>,
    pub failure: Option<String>,
    pub connection_error: Option<String>,
    pub forget_target: Option<Target>,
    received_ms: Option<u64>,
}
impl PanelModel {
    pub fn observe(&mut self, s: Snapshot, caps: Value, now_ms: u64) -> Result<(), String> {
        if s.schema_version != 1 {
            self.unavailable("Unsupported status schema".into());
            return Err("Unsupported status schema".into());
        }
        if self.snapshot.as_ref().is_some_and(|old| {
            old.daemon_instance == s.daemon_instance && s.revision < old.revision
        }) {
            self.unavailable("Daemon status moved backwards".into());
            return Err("Daemon status moved backwards".into());
        }
        if self
            .forget_target
            .as_ref()
            .is_some_and(|t| *t != Target::from_snapshot(&s))
        {
            self.forget_target = None;
        }
        self.snapshot = Some(s);
        self.capabilities = caps;
        self.reachable = true;
        self.connection_error = None;
        self.received_ms = Some(now_ms);
        Ok(())
    }
    pub fn unavailable(&mut self, reason: String) {
        self.reachable = false;
        self.connection_error = Some(reason);
        self.forget_target = None;
    }
    pub fn current(&self, now_ms: u64) -> bool {
        self.reachable
            && self
                .received_ms
                .is_some_and(|t| now_ms.saturating_sub(t) <= STATUS_TTL_MS)
    }
    pub fn can(&self, a: Action, now_ms: u64) -> bool {
        !self.pending
            && self.current(now_ms)
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| validate_action(s, &self.capabilities, a).is_ok())
    }
    pub fn prepare(&mut self, a: Action, now_ms: u64) -> Result<Command, String> {
        if !self.can(a, now_ms) {
            return Err(
                "Control unavailable: wait for a current daemon status and supported device."
                    .into(),
            );
        }
        let target = Target::from_snapshot(self.snapshot.as_ref().ok_or("No device")?);
        if a == Action::Forget && self.forget_target.as_ref() != Some(&target) {
            return Err("Confirm removal of this selected device first.".into());
        }
        self.pending = true;
        self.forget_target = None;
        self.failure = None;
        self.message = Some("Waiting for daemon confirmation…".into());
        Ok(Command { target, action: a })
    }
    pub fn finish(&mut self, result: Result<(), String>) {
        self.pending = false;
        match result {
            Ok(()) => {
                self.message = None;
                self.failure = None;
            }
            Err(e) => {
                self.message = None;
                self.failure = Some(e);
            }
        }
    }
    pub fn confirm_forget(&mut self, now_ms: u64) {
        if self.can(Action::Forget, now_ms) {
            self.forget_target = self.snapshot.as_ref().map(Target::from_snapshot);
        }
    }
}

pub fn fresh(s: &Snapshot, field: &str) -> bool {
    s.device
        .observations
        .get(field)
        .is_some_and(|m| !m.stale && m.generation == s.session_generation)
}
pub fn control(s: &Snapshot, id: u8) -> Option<u8> {
    if !s.connected || !fresh(s, &format!("control.{id:02x}")) {
        return None;
    }
    s.device.reported_controls.get(&id).map(|v| v[0])
}
pub fn toggle(s: &Snapshot, id: u8) -> Option<bool> {
    match control(s, id) {
        Some(1) => Some(true),
        Some(2) => Some(false),
        _ => None,
    }
}
pub fn mode(s: &Snapshot) -> Option<NoiseControlMode> {
    (s.connected && fresh(s, "noise_control"))
        .then_some(s.device.listening_mode)
        .flatten()
}

pub fn validate_action(s: &Snapshot, caps: &Value, a: Action) -> Result<(), String> {
    if s.schema_version != 1 || s.device.address.is_empty() {
        return Err("No selected device".into());
    }
    let c = s.device.model.capabilities();
    let allowed = match a {
        Action::Connect => {
            caps["device_management"] == true && s.adapter_ready && !s.bluez_connected
        }
        Action::Disconnect => {
            caps["device_management"] == true && s.adapter_ready && s.bluez_connected
        }
        Action::Forget => caps["device_management"] == true && s.adapter_ready,
        Action::EarPolicy(_) => true,
        Action::Mode(m) => {
            s.connected
                && fresh(s, "metadata")
                && c.noise_control
                && match m {
                    NoiseControlMode::Off => c.noise_off,
                    NoiseControlMode::Adaptive => c.adaptive,
                    _ => true,
                }
        }
        Action::Conversation(_) => {
            s.connected
                && fresh(s, "metadata")
                && c.conversational_awareness
                && toggle(s, 0x28).is_some()
        }
        Action::OneBud(_) => {
            s.connected && fresh(s, "metadata") && c.one_bud_anc && toggle(s, 0x1b).is_some()
        }
        Action::Adaptive(v) => {
            s.connected
                && fresh(s, "metadata")
                && c.adaptive
                && mode(s) == Some(NoiseControlMode::Adaptive)
                && control(s, 0x2e).is_some_and(|v| v <= 100)
                && v <= 100
        }
    };
    if allowed {
        Ok(())
    } else {
        Err("Device state or daemon capability does not support this control yet.".into())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct BatteryView {
    pub label: String,
    pub percent: Option<u8>,
    pub detail: &'static str,
    pub stale: bool,
}
pub fn battery(
    s: &Snapshot,
    label: &str,
    key: &str,
    b: BatteryState,
    current: bool,
) -> BatteryView {
    let known = available(b) && b.level <= 100 && s.device.observations.contains_key(key);
    let stale = known && (!current || !fresh(s, key));
    BatteryView {
        label: label.into(),
        percent: known.then_some(b.level),
        stale,
        detail: if !known {
            "Not reported"
        } else if stale {
            "Last reported"
        } else {
            match b.status {
                BatteryStatus::Charging => "Charging",
                BatteryStatus::OptimizedCharging => "Charge paused",
                _ => "On battery",
            }
        },
    }
}

/// Human-readable hardware labels; no firmware inference.
pub fn model_name(model: librepods_core::device::state::AirPodsModel) -> &'static str {
    use librepods_core::device::state::AirPodsModel::*;
    match model {
        Unknown => "Unknown AirPods model",
        AirPods1 => "AirPods (1st generation)",
        AirPods2 => "AirPods (2nd generation)",
        AirPods3 => "AirPods (3rd generation)",
        AirPods4 => "AirPods (4th generation)",
        AirPods4Anc => "AirPods 4 with ANC",
        AirPodsPro => "AirPods Pro",
        AirPodsPro2Lightning => "AirPods Pro 2 · Lightning",
        AirPodsPro2Usbc => "AirPods Pro 2 · USB-C",
        AirPodsPro3 => "AirPods Pro 3",
        AirPodsMaxLightning => "AirPods Max · Lightning",
        AirPodsMaxUsbc => "AirPods Max · USB-C",
        AirPodsMax2 => "AirPods Max 2",
    }
}
