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
