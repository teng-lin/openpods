// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Deterministic command confirmation; the daemon owns I/O and time.

use crate::device::state::{DeviceState, NoiseControlMode, ObservationSource};
use crate::protocol::control_command::{ControlCommand, ControlCommandId};
use crate::protocol::{packets, parser::ParsedPacket};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingChange {
    ListeningMode(NoiseControlMode),
    ConversationalAwareness(bool),
    SingleEarbudAnc(bool),
    AdaptiveNoiseLevel(u8),
    Rename(String),
    LongPressModes(u8),
}

impl std::fmt::Display for SettingChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ListeningMode(mode) => write!(f, "listening mode {mode:?}"),
            Self::ConversationalAwareness(on) => write!(
                f,
                "Conversational Awareness {}",
                if *on { "on" } else { "off" }
            ),
            Self::SingleEarbudAnc(on) => {
                write!(f, "single-earbud ANC {}", if *on { "on" } else { "off" })
            }
            Self::AdaptiveNoiseLevel(value) => write!(f, "adaptive noise level {value}"),
            Self::Rename(_) => write!(f, "device name"),
            Self::LongPressModes(mask) => write!(f, "long-press listening modes {mask:#04x}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum CommandRejection {
    #[error("the device has not reported a supported model")]
    UnknownModel,
    #[error("the selected model does not support this setting")]
    Unsupported,
    #[error("the setting value is out of range")]
    InvalidValue,
    #[error("adaptive noise level requires confirmed Adaptive listening mode")]
    NotAdaptive,
}

impl SettingChange {
    pub fn id(&self) -> Option<u8> {
        Some(match self {
            Self::ListeningMode(_) => ControlCommandId::ListeningMode.as_byte(),
            Self::ConversationalAwareness(_) => {
                ControlCommandId::ConversationDetectConfig.as_byte()
            }
            Self::SingleEarbudAnc(_) => ControlCommandId::OneBudAncMode.as_byte(),
            Self::AdaptiveNoiseLevel(_) => ControlCommandId::AutoAncStrength.as_byte(),
            Self::LongPressModes(_) => ControlCommandId::ListeningModeConfigs.as_byte(),
            Self::Rename(_) => return None,
        })
    }

    pub fn validate(&self, state: &DeviceState) -> Result<(), CommandRejection> {
        use crate::device::state::AirPodsModel;
        if state.model == AirPodsModel::Unknown {
            return Err(CommandRejection::UnknownModel);
        }
        let caps = state.capabilities();
        match self {
            Self::ListeningMode(mode)
                if !caps.noise_control
                    || (*mode == NoiseControlMode::Off && !caps.noise_off)
                    || (*mode == NoiseControlMode::Adaptive && !caps.adaptive) =>
            {
                Err(CommandRejection::Unsupported)
            }
            Self::ConversationalAwareness(_) if !caps.conversational_awareness => {
                Err(CommandRejection::Unsupported)
            }
            Self::SingleEarbudAnc(_) if !caps.one_bud_anc => Err(CommandRejection::Unsupported),
            Self::AdaptiveNoiseLevel(_) if !caps.adaptive => Err(CommandRejection::Unsupported),
            Self::AdaptiveNoiseLevel(value) if *value > 100 => Err(CommandRejection::InvalidValue),
            Self::AdaptiveNoiseLevel(_)
                if !state.noise_control_reported
                    || state.noise_control != NoiseControlMode::Adaptive =>
            {
                Err(CommandRejection::NotAdaptive)
            }
            Self::Rename(name)
                if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) =>
            {
                Err(CommandRejection::InvalidValue)
            }
            Self::LongPressModes(mask) if *mask == 0 || *mask > 15 => {
                Err(CommandRejection::InvalidValue)
            }
            Self::LongPressModes(mask)
                if !caps.noise_control
                    || (*mask & 1 != 0 && !caps.noise_off)
                    || (*mask & 8 != 0 && !caps.adaptive) =>
            {
                Err(CommandRejection::Unsupported)
            }
            _ => Ok(()),
        }
    }

    /// These configuration reports are startup snapshots on verified Pro 2 firmware.
    /// Reopening the control channel obtains readback without replaying a mutation.
    pub fn requires_readback_reopen(&self) -> bool {
        matches!(
            self,
            Self::ConversationalAwareness(_)
                | Self::SingleEarbudAnc(_)
                | Self::AdaptiveNoiseLevel(_)
        )
    }

    pub fn packet(&self) -> Vec<u8> {
        if let Self::Rename(name) = self {
            return packets::build_rename(name);
        }
        ControlCommand::create(self.id().unwrap_or_default(), &self.target()).to_vec()
    }

    fn target(&self) -> [u8; 4] {
        let value = match self {
            Self::ListeningMode(mode) => mode.as_byte(),
            Self::ConversationalAwareness(on) | Self::SingleEarbudAnc(on) => {
                if *on {
                    1
                } else {
                    2
                }
            }
            Self::AdaptiveNoiseLevel(value) | Self::LongPressModes(value) => *value,
            Self::Rename(_) => 0,
        };
        [value, 0, 0, 0]
    }

    fn reported(&self, packet: &ParsedPacket) -> Option<bool> {
        match (self, packet) {
            (Self::Rename(target), ParsedPacket::Metadata { name, .. }) => Some(target == name),
            (Self::ListeningMode(target), ParsedPacket::NoiseControl(mode)) => Some(target == mode),
            (_, ParsedPacket::ControlCommand { id, data }) if Some(id.as_byte()) == self.id() => {
                Some(*data == self.target())
            }
            _ => None,
        }
    }

    fn current(&self, state: &DeviceState) -> Option<bool> {
        match self {
            Self::Rename(target) if state.fresh("metadata") => Some(target == &state.device_name),
            Self::ListeningMode(target)
                if state.noise_control_reported && state.fresh("noise_control") =>
            {
                Some(*target == state.noise_control)
            }
            _ => self.id().and_then(|id| {
                if !state.fresh(&format!("control.{id:02x}")) {
                    return None;
                }
                state
                    .reported_controls
                    .get(&id)
                    .map(|value| *value == self.target())
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Confirmed,
    AlreadySet,
    TimedOut,
    OutcomeUnknown,
    Cancelled,
    Superseded,
    Rejected(CommandRejection),
}

pub type ChangeOutcome = CommandOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    Wait,
    Send(Vec<u8>),
    ReopenForReadback { generation: u64 },
    Complete(CommandOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    New,
    Baseline,
    Dispatch,
    Confirmation,
    ReadbackReopen { generation: u64 },
    ReadbackConfirmation { started_at_ms: u64 },
    Complete(CommandOutcome),
}

/// One command, scoped to a connection generation and an absolute deadline.
pub struct PendingChange {
    pub change: SettingChange,
    pub generation: u64,
    pub deadline_ms: u64,
    phase: Phase,
    readback_identity: Option<(String, String)>,
}

impl PendingChange {
    pub fn new(change: SettingChange, generation: u64, deadline_ms: u64) -> Self {
        Self {
            change,
            generation,
            deadline_ms,
            phase: Phase::New,
            readback_identity: None,
        }
    }

    fn finish(&mut self, outcome: CommandOutcome) -> CommandAction {
        self.phase = Phase::Complete(outcome);
        CommandAction::Complete(outcome)
    }

    fn dispatch(&mut self) -> CommandAction {
        self.phase = Phase::Dispatch;
        CommandAction::Send(self.change.packet())
    }

    pub fn begin(&mut self, state: &DeviceState, now_ms: u64) -> CommandAction {
        let action = self.tick(state.session_generation, now_ms);
        if action != CommandAction::Wait {
            return action;
        }
        if self.phase != Phase::New {
            return CommandAction::Wait;
        }
        if let Err(reason) = self.change.validate(state) {
            return self.finish(CommandOutcome::Rejected(reason));
        }
        self.readback_identity =
            Some((state.bluetooth_address.clone(), state.model_number.clone()));
        match self.change.current(state) {
            // Startup-only config caches cannot observe changes made by another host.
            // A new explicit request still writes its desired value once and verifies it.
            Some(true) if self.change.requires_readback_reopen() => self.dispatch(),
            Some(true) => self.finish(CommandOutcome::AlreadySet),
            Some(false) => self.dispatch(),
            None => {
                self.phase = Phase::Baseline;
                CommandAction::Wait
            }
        }
    }

    pub fn on_packet(
        &mut self,
        packet: &ParsedPacket,
        generation: u64,
        now_ms: u64,
    ) -> CommandAction {
        let action = self.tick(generation, now_ms);
        if action != CommandAction::Wait {
            return action;
        }
        match (self.phase, self.change.reported(packet)) {
            (Phase::Baseline, Some(true)) if self.change.requires_readback_reopen() => {
                self.dispatch()
            }
            (Phase::Baseline, Some(true)) => self.finish(CommandOutcome::AlreadySet),
            (Phase::Baseline, Some(false)) => self.dispatch(),
            (Phase::Confirmation, Some(true)) => self.finish(CommandOutcome::Confirmed),
            _ => CommandAction::Wait,
        }
    }

    pub fn mark_sent(&mut self, generation: u64, now_ms: u64) -> CommandAction {
        if self.phase == Phase::Dispatch {
            // A completed write remains uncertain even if its completion reaches the deadline.
            self.phase = Phase::Confirmation;
            let action = self.tick(generation, now_ms);
            if action != CommandAction::Wait {
                return action;
            }
            if self.change.requires_readback_reopen() {
                let Some(next) = self.generation.checked_add(1) else {
                    return self.finish(CommandOutcome::OutcomeUnknown);
                };
                self.phase = Phase::ReadbackReopen { generation: next };
                return CommandAction::ReopenForReadback { generation: next };
            }
        }
        self.tick(generation, now_ms)
    }

    /// Bind the sole planned new generation. Ordinary generation changes still cancel.
    /// This phase can observe state but can never dispatch the setting again.
    pub fn mark_readback_reopened(&mut self, generation: u64, now_ms: u64) -> CommandAction {
        let action = self.tick(self.generation, now_ms);
        if action != CommandAction::Wait {
            return action;
        }
        if self.phase != (Phase::ReadbackReopen { generation }) {
            return self.cancel();
        }
        self.generation = generation;
        self.phase = Phase::ReadbackConfirmation {
            started_at_ms: now_ms,
        };
        CommandAction::Wait
    }

    /// Observe reducer-owned state so config-before-metadata ordering is safe.
    pub fn observe_readback(&mut self, state: &DeviceState, now_ms: u64) -> CommandAction {
        let action = self.tick(state.session_generation, now_ms);
        if action != CommandAction::Wait {
            return action;
        }
        let Phase::ReadbackConfirmation { started_at_ms } = self.phase else {
            return CommandAction::Wait;
        };
        let fresh_aacp = |field: &str| {
            state.connected
                && state.observations.get(field).is_some_and(|meta| {
                    !meta.stale
                        && meta.generation == self.generation
                        && meta.source == ObservationSource::Aacp
                        && meta.observed_at_ms >= started_at_ms
                })
        };
        let Some((address, model)) = &self.readback_identity else {
            return self.cancel();
        };
        if address.is_empty()
            || model.is_empty()
            || !address.eq_ignore_ascii_case(&state.bluetooth_address)
        {
            return self.cancel();
        }
        if !fresh_aacp("metadata") {
            return CommandAction::Wait;
        }
        if *model != state.model_number {
            return self.cancel();
        }
        if let Some(id) = self.change.id() {
            if fresh_aacp(&format!("control.{id:02x}")) && self.change.current(state) == Some(true)
            {
                return self.finish(CommandOutcome::Confirmed);
            }
        }
        CommandAction::Wait
    }

    fn written(&self) -> bool {
        matches!(
            self.phase,
            Phase::Confirmation | Phase::ReadbackReopen { .. } | Phase::ReadbackConfirmation { .. }
        )
    }

    pub fn tick(&mut self, generation: u64, now_ms: u64) -> CommandAction {
        if let Phase::Complete(outcome) = self.phase {
            return CommandAction::Complete(outcome);
        }
        if generation != self.generation {
            return self.transport_lost();
        }
        if now_ms >= self.deadline_ms {
            return self.finish(if self.written() {
                CommandOutcome::OutcomeUnknown
            } else {
                CommandOutcome::TimedOut
            });
        }
        CommandAction::Wait
    }

    pub fn cancel(&mut self) -> CommandAction {
        if let Phase::Complete(outcome) = self.phase {
            return CommandAction::Complete(outcome);
        }
        self.finish(if self.written() {
            CommandOutcome::OutcomeUnknown
        } else {
            CommandOutcome::Cancelled
        })
    }

    pub fn supersede(&mut self) -> CommandAction {
        if let Phase::Complete(outcome) = self.phase {
            return CommandAction::Complete(outcome);
        }
        self.finish(if self.written() {
            CommandOutcome::OutcomeUnknown
        } else {
            CommandOutcome::Superseded
        })
    }

    pub fn transport_lost(&mut self) -> CommandAction {
        self.cancel()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::state::AirPodsModel;
    use crate::protocol::parser;
    fn state() -> DeviceState {
        let mut state = DeviceState {
            model: AirPodsModel::AirPodsPro2Usbc,
            ..DeviceState::default()
        };
        state.set_device_identity("00:11:22:33:44:55", "Pods");
        state.begin_session(1);
        state.apply_packet(
            &ParsedPacket::Metadata {
                name: "Pods".into(),
                model_number: "A3048".into(),
                manufacturer: "Apple Inc.".into(),
            },
            1,
            0,
        );
        state
    }
    fn mode(mode: NoiseControlMode) -> ParsedPacket {
        ParsedPacket::NoiseControl(mode)
    }
    fn control(id: ControlCommandId, value: u8) -> ParsedPacket {
        ParsedPacket::ControlCommand {
            id,
            data: [value, 0, 0, 0],
        }
    }
    #[test]
    fn config_echo_in_the_written_generation_cannot_complete_reopen_readback() {
        let mut state = state();
        state.apply_packet(&control(ControlCommandId::OneBudAncMode, 2), 1, 0);
        let mut pending = PendingChange::new(SettingChange::SingleEarbudAnc(true), 1, 100);
        assert!(matches!(pending.begin(&state, 1), CommandAction::Send(_)));
        pending.mark_sent(1, 2);
        assert_ne!(
            pending.on_packet(&control(ControlCommandId::OneBudAncMode, 1), 1, 3),
            CommandAction::Complete(CommandOutcome::Confirmed)
        );
    }
    #[test]
    fn setting_change_requires_fresh_confirmation_after_dispatch() {
        let mut state = state();
        state.apply_packet(&mode(NoiseControlMode::NoiseCancellation), 1, 0);
        let change = SettingChange::ListeningMode(NoiseControlMode::Transparency);
        let mut pending = PendingChange::new(change.clone(), 1, 100);
        assert_eq!(
            pending.begin(&state, 1),
            CommandAction::Send(
                packets::build_noise_control(NoiseControlMode::Transparency).to_vec()
            )
        );
        assert_eq!(
            pending.on_packet(&mode(NoiseControlMode::Transparency), 1, 2),
            CommandAction::Wait
        );
        assert_eq!(pending.mark_sent(1, 3), CommandAction::Wait);
        assert_eq!(
            pending.on_packet(&control(ControlCommandId::OneBudAncMode, 1), 1, 4),
            CommandAction::Wait
        );
        assert_eq!(
            pending.on_packet(&mode(NoiseControlMode::NoiseCancellation), 1, 5),
            CommandAction::Wait
        );
        assert_eq!(
            pending.on_packet(&mode(NoiseControlMode::Transparency), 1, 6),
            CommandAction::Complete(CommandOutcome::Confirmed)
        );
        assert_eq!(
            pending.tick(1, 200),
            CommandAction::Complete(CommandOutcome::Confirmed)
        );
        assert_eq!(change.id(), Some(ControlCommandId::ListeningMode.as_byte()));
        assert!(!change.to_string().is_empty());
    }
    #[test]
    fn missing_config_baseline_waits_and_new_desire_is_verified_by_reopen() {
        let state = state();
        let mut pending = PendingChange::new(SettingChange::ConversationalAwareness(true), 1, 100);
        assert_eq!(
            pending.on_packet(
                &control(ControlCommandId::ConversationDetectConfig, 1),
                1,
                0
            ),
            CommandAction::Wait
        );
        assert_eq!(pending.begin(&state, 1), CommandAction::Wait);
        assert_eq!(pending.begin(&state, 2), CommandAction::Wait);
        assert_eq!(
            pending.on_packet(
                &control(ControlCommandId::ConversationDetectConfig, 1),
                1,
                3
            ),
            CommandAction::Send(ControlCommand::enabled(0x28).to_vec())
        );
        let mut pending = PendingChange::new(SettingChange::SingleEarbudAnc(false), 1, 100);
        assert_eq!(pending.begin(&state, 1), CommandAction::Wait);
        assert_eq!(
            pending.on_packet(&control(ControlCommandId::OneBudAncMode, 1), 1, 2),
            CommandAction::Send(ControlCommand::disabled(0x1b).to_vec())
        );
        assert_eq!(
            pending.mark_sent(1, 3),
            CommandAction::ReopenForReadback { generation: 2 }
        );
        assert_eq!(
            pending.on_packet(&control(ControlCommandId::OneBudAncMode, 2), 1, 4),
            CommandAction::Wait
        );
        pending.mark_readback_reopened(2, 5);
        let mut fresh = state.clone();
        fresh.begin_session(2);
        fresh.apply_packet(&control(ControlCommandId::OneBudAncMode, 2), 2, 6);
        fresh.apply_packet(
            &ParsedPacket::Metadata {
                name: "Pods".into(),
                model_number: "A3048".into(),
                manufacturer: "Apple Inc.".into(),
            },
            2,
            7,
        );
        assert_eq!(
            pending.observe_readback(&fresh, 7),
            CommandAction::Complete(CommandOutcome::Confirmed)
        );
    }
    #[test]
    fn queued_expiry_supersession_and_disconnect_never_replay_mutations() {
        let state = state();
        let change = SettingChange::ConversationalAwareness(false);
        let mut pending = PendingChange::new(change.clone(), 1, 0);
        assert_eq!(
            pending.begin(&state, 0),
            CommandAction::Complete(CommandOutcome::TimedOut)
        );
        let mut pending = PendingChange::new(change.clone(), 1, 100);
        assert_eq!(
            pending.cancel(),
            CommandAction::Complete(CommandOutcome::Cancelled)
        );
        let mut pending = PendingChange::new(change.clone(), 1, 100);
        assert_eq!(
            pending.supersede(),
            CommandAction::Complete(CommandOutcome::Superseded)
        );
        let mut pending = PendingChange::new(change.clone(), 1, 100);
        assert_eq!(
            pending.tick(2, 0),
            CommandAction::Complete(CommandOutcome::Cancelled)
        );
        for action in 0..4 {
            let mut pending = PendingChange::new(change.clone(), 1, 100);
            pending.begin(&state, 0);
            pending.on_packet(
                &control(ControlCommandId::ConversationDetectConfig, 1),
                1,
                1,
            );
            pending.mark_sent(1, 2);
            let outcome = match action {
                0 => pending.tick(1, 100),
                1 => pending.cancel(),
                2 => pending.supersede(),
                _ => pending.transport_lost(),
            };
            assert_eq!(
                outcome,
                CommandAction::Complete(CommandOutcome::OutcomeUnknown)
            );
        }
        let mut pending = PendingChange::new(change, 1, 100);
        pending.begin(&state, 0);
        pending.on_packet(
            &control(ControlCommandId::ConversationDetectConfig, 1),
            1,
            1,
        );
        assert_eq!(
            pending.mark_sent(1, 100),
            CommandAction::Complete(CommandOutcome::OutcomeUnknown)
        );
    }
    #[test]
    fn cached_startup_target_never_skips_a_new_explicit_configuration_request() {
        let mut state = state();
        state.apply_packet(
            &control(ControlCommandId::ConversationDetectConfig, 1),
            1,
            1,
        );
        let mut pending = PendingChange::new(SettingChange::ConversationalAwareness(true), 1, 100);
        assert_eq!(
            pending.begin(&state, 2),
            CommandAction::Send(ControlCommand::enabled(0x28).to_vec())
        );
        assert_eq!(
            pending.mark_sent(1, 3),
            CommandAction::ReopenForReadback { generation: 2 }
        );
    }
    #[test]
    fn readback_requires_one_designated_new_generation_and_fresh_aacp_identity_and_target() {
        let mut original = state();
        original.apply_packet(
            &control(ControlCommandId::ConversationDetectConfig, 2),
            1,
            1,
        );
        let mut pending = PendingChange::new(SettingChange::ConversationalAwareness(true), 1, 100);
        assert!(matches!(
            pending.begin(&original, 2),
            CommandAction::Send(_)
        ));
        assert_eq!(
            pending.mark_sent(1, 3),
            CommandAction::ReopenForReadback { generation: 2 }
        );
        assert_eq!(pending.mark_sent(1, 4), CommandAction::Wait);
        assert_eq!(pending.mark_readback_reopened(2, 5), CommandAction::Wait);
        let mut fresh = original.clone();
        fresh.begin_session(2);
        fresh.apply_packet(
            &control(ControlCommandId::ConversationDetectConfig, 1),
            2,
            6,
        );
        assert_eq!(pending.observe_readback(&fresh, 6), CommandAction::Wait); // Old metadata stayed stale.
        assert_eq!(pending.begin(&fresh, 6), CommandAction::Wait); // Readback-only, never another mutation.
        fresh.apply_packet(
            &ParsedPacket::Metadata {
                name: "Pods".into(),
                model_number: "A3048".into(),
                manufacturer: "Apple Inc.".into(),
            },
            2,
            7,
        );
        fresh.observations.get_mut("control.28").unwrap().source = ObservationSource::Ble;
        assert_eq!(pending.observe_readback(&fresh, 7), CommandAction::Wait);
        fresh.apply_packet(
            &control(ControlCommandId::ConversationDetectConfig, 1),
            2,
            8,
        );
        assert_eq!(
            pending.observe_readback(&fresh, 8),
            CommandAction::Complete(CommandOutcome::Confirmed)
        );
    }
    #[test]
    fn invalid_readback_generation_identity_missing_report_and_expiry_remain_unknown() {
        for failure in 0..5 {
            let mut original = state();
            original.apply_packet(&control(ControlCommandId::OneBudAncMode, 2), 1, 1);
            let mut pending = PendingChange::new(SettingChange::SingleEarbudAnc(true), 1, 100);
            pending.begin(&original, 2);
            pending.mark_sent(1, 3);
            if failure == 0 {
                assert_eq!(
                    pending.mark_readback_reopened(3, 4),
                    CommandAction::Complete(CommandOutcome::OutcomeUnknown)
                );
                continue;
            }
            pending.mark_readback_reopened(2, 4);
            let mut fresh = original.clone();
            fresh.begin_session(2);
            if failure == 1 {
                fresh.set_device_identity("00:11:22:33:44:66", "Other");
            }
            fresh.apply_packet(
                &ParsedPacket::Metadata {
                    name: "Pods".into(),
                    model_number: if failure == 2 { "A2698" } else { "A3048" }.into(),
                    manufacturer: "Apple Inc.".into(),
                },
                2,
                5,
            );
            if failure != 3 {
                fresh.apply_packet(
                    &control(
                        ControlCommandId::OneBudAncMode,
                        if failure == 4 { 2 } else { 1 },
                    ),
                    2,
                    6,
                );
            }
            let observed = pending.observe_readback(&fresh, 7);
            if matches!(failure, 1 | 2) {
                assert_eq!(
                    observed,
                    CommandAction::Complete(CommandOutcome::OutcomeUnknown)
                );
            } else {
                assert_eq!(observed, CommandAction::Wait);
                assert_eq!(
                    pending.tick(2, 100),
                    CommandAction::Complete(CommandOutcome::OutcomeUnknown)
                );
            }
            assert!(!matches!(
                pending.begin(&fresh, 101),
                CommandAction::Send(_)
            ));
        }
    }
    #[test]
    fn model_and_value_validation_prevent_unsupported_mutations() {
        let mut state = state();
        for change in [
            SettingChange::ListeningMode(NoiseControlMode::Off),
            SettingChange::ConversationalAwareness(true),
            SettingChange::SingleEarbudAnc(true),
            SettingChange::Rename("Pods".into()),
            SettingChange::LongPressModes(15),
        ] {
            assert!(change.validate(&state).is_ok());
            assert!(!change.packet().is_empty());
            assert!(!change.to_string().is_empty());
        }
        assert_eq!(
            SettingChange::AdaptiveNoiseLevel(101).validate(&state),
            Err(CommandRejection::InvalidValue)
        );
        assert_eq!(
            SettingChange::AdaptiveNoiseLevel(50).validate(&state),
            Err(CommandRejection::NotAdaptive)
        );
        state.apply_packet(&mode(NoiseControlMode::Adaptive), 1, 1);
        assert!(SettingChange::AdaptiveNoiseLevel(50)
            .validate(&state)
            .is_ok());
        assert_eq!(
            SettingChange::AdaptiveNoiseLevel(50).packet(),
            ControlCommand::create(0x2e, &[50, 0, 0, 0])
        );
        assert!(!SettingChange::AdaptiveNoiseLevel(50).to_string().is_empty());
        for change in [
            SettingChange::Rename(String::new()),
            SettingChange::Rename("a".repeat(256)),
            SettingChange::Rename("bad\nname".into()),
            SettingChange::LongPressModes(0),
            SettingChange::LongPressModes(16),
        ] {
            assert_eq!(change.validate(&state), Err(CommandRejection::InvalidValue));
        }
        state.model = AirPodsModel::AirPodsPro3;
        assert_eq!(
            SettingChange::ListeningMode(NoiseControlMode::Off).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        assert_eq!(
            SettingChange::LongPressModes(1).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        state.model = AirPodsModel::AirPodsPro;
        assert_eq!(
            SettingChange::ListeningMode(NoiseControlMode::Adaptive).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        assert_eq!(
            SettingChange::ConversationalAwareness(true).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        assert_eq!(
            SettingChange::AdaptiveNoiseLevel(50).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        assert_eq!(
            SettingChange::LongPressModes(8).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        state.model = AirPodsModel::AirPods1;
        assert_eq!(
            SettingChange::ListeningMode(NoiseControlMode::NoiseCancellation).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        assert_eq!(
            SettingChange::SingleEarbudAnc(false).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        assert_eq!(
            SettingChange::LongPressModes(2).validate(&state),
            Err(CommandRejection::Unsupported)
        );
        state.model = AirPodsModel::Unknown;
        let mut pending = PendingChange::new(SettingChange::ConversationalAwareness(true), 1, 100);
        assert_eq!(
            pending.begin(&state, 0),
            CommandAction::Complete(CommandOutcome::Rejected(CommandRejection::UnknownModel))
        );
    }
    #[test]
    fn cached_fresh_values_and_metadata_confirmations_are_supported() {
        let mut state = state();
        state.apply_packet(&mode(NoiseControlMode::Transparency), 1, 1);
        let mut pending = PendingChange::new(
            SettingChange::ListeningMode(NoiseControlMode::Transparency),
            1,
            100,
        );
        assert_eq!(
            pending.begin(&state, 2),
            CommandAction::Complete(CommandOutcome::AlreadySet)
        );
        state.apply_packet(
            &ParsedPacket::Metadata {
                name: "Old".into(),
                model_number: "A3048".into(),
                manufacturer: "Apple".into(),
            },
            1,
            3,
        );
        let mut pending = PendingChange::new(SettingChange::Rename("New".into()), 1, 100);
        assert!(matches!(pending.begin(&state, 4), CommandAction::Send(_)));
        pending.mark_sent(1, 5);
        assert_eq!(
            pending.on_packet(
                &ParsedPacket::Metadata {
                    name: "New".into(),
                    model_number: "A3048".into(),
                    manufacturer: "Apple".into()
                },
                1,
                6
            ),
            CommandAction::Complete(CommandOutcome::Confirmed)
        );
        let raw = ControlCommand::enabled(0x28);
        assert!(matches!(
            parser::parse(&raw),
            Ok(ParsedPacket::ControlCommand { .. })
        ));
    }
}
