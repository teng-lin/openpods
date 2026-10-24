// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Deterministic command confirmation; the daemon owns I/O and time.

use crate::device::state::{DeviceState, NoiseControlMode};
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
    Complete(CommandOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    New,
    Baseline,
    Dispatch,
    Confirmation,
    Complete(CommandOutcome),
}

/// One command, scoped to a connection generation and an absolute deadline.
pub struct PendingChange {
    pub change: SettingChange,
    pub generation: u64,
    pub deadline_ms: u64,
    phase: Phase,
}

impl PendingChange {
    pub fn new(change: SettingChange, generation: u64, deadline_ms: u64) -> Self {
        Self {
            change,
            generation,
            deadline_ms,
            phase: Phase::New,
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
        match self.change.current(state) {
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
            (Phase::Baseline, Some(true)) => self.finish(CommandOutcome::AlreadySet),
            (Phase::Baseline, Some(false)) => self.dispatch(),
            (Phase::Confirmation, Some(true)) => self.finish(CommandOutcome::Confirmed),
            _ => CommandAction::Wait,
        }
    }

    pub fn mark_sent(&mut self, generation: u64, now_ms: u64) -> CommandAction {
        let action = self.tick(generation, now_ms);
        if action != CommandAction::Wait {
            return action;
        }
        if self.phase == Phase::Dispatch {
            self.phase = Phase::Confirmation;
        }
        CommandAction::Wait
    }

    pub fn tick(&mut self, generation: u64, now_ms: u64) -> CommandAction {
        if let Phase::Complete(outcome) = self.phase {
            return CommandAction::Complete(outcome);
        }
        if generation != self.generation {
            return self.transport_lost();
        }
        if now_ms >= self.deadline_ms {
            return self.finish(if self.phase == Phase::Confirmation {
                CommandOutcome::OutcomeUnknown
            } else {
                CommandOutcome::TimedOut
            });
        }
        CommandAction::Wait
    }

    pub fn cancel(&mut self) -> CommandAction {
        self.finish(if self.phase == Phase::Confirmation {
            CommandOutcome::OutcomeUnknown
        } else {
            CommandOutcome::Cancelled
        })
    }

    pub fn supersede(&mut self) -> CommandAction {
        self.finish(if self.phase == Phase::Confirmation {
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
        state.begin_session(1);
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
    fn missing_baseline_waits_and_matching_baseline_needs_no_write() {
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
            CommandAction::Complete(CommandOutcome::AlreadySet)
        );
        let mut pending = PendingChange::new(SettingChange::SingleEarbudAnc(false), 1, 100);
        assert_eq!(pending.begin(&state, 1), CommandAction::Wait);
        assert_eq!(
            pending.on_packet(&control(ControlCommandId::OneBudAncMode, 1), 1, 2),
            CommandAction::Send(ControlCommand::disabled(0x1b).to_vec())
        );
        pending.mark_sent(1, 3);
        assert_eq!(
            pending.on_packet(&control(ControlCommandId::OneBudAncMode, 2), 1, 4),
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
            CommandAction::Complete(CommandOutcome::TimedOut)
        );
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
        state.apply_packet(
            &control(ControlCommandId::ConversationDetectConfig, 1),
            1,
            1,
        );
        let mut pending = PendingChange::new(SettingChange::ConversationalAwareness(true), 1, 100);
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
