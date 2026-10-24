// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded, confirmed changes to supported AirPods settings.

use std::time::{Duration, Instant};

use thiserror::Error;

use crate::device::state::NoiseControlMode;
use crate::protocol::control_command::{ControlCommand, ControlCommandId};
use crate::protocol::parser::{self, ParsedPacket};
use crate::transport::{L2capTransport, TransportError};

#[derive(Debug, Clone, Copy)]
pub enum SettingChange {
    ListeningMode(NoiseControlMode),
    ConversationalAwareness(bool),
    SingleEarbudAnc(bool),
}

impl std::fmt::Display for SettingChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ListeningMode(mode) => write!(formatter, "listening mode {mode:?}"),
            Self::ConversationalAwareness(enabled) => write!(
                formatter,
                "Conversational Awareness {}",
                if *enabled { "on" } else { "off" }
            ),
            Self::SingleEarbudAnc(enabled) => write!(
                formatter,
                "single-earbud ANC {}",
                if *enabled { "on" } else { "off" }
            ),
        }
    }
}

impl SettingChange {
    fn command(self) -> (ControlCommandId, [u8; 4]) {
        match self {
            Self::ListeningMode(mode) => {
                (ControlCommandId::ListeningMode, [mode.as_byte(), 0, 0, 0])
            }
            Self::ConversationalAwareness(enabled) => (
                ControlCommandId::ConversationDetectConfig,
                [if enabled { 1 } else { 2 }, 0, 0, 0],
            ),
            Self::SingleEarbudAnc(enabled) => (
                ControlCommandId::OneBudAncMode,
                [if enabled { 1 } else { 2 }, 0, 0, 0],
            ),
        }
    }

    fn reported_value(self, packet: &ParsedPacket) -> Option<[u8; 4]> {
        match packet {
            ParsedPacket::NoiseControl(mode) if matches!(self, Self::ListeningMode(_)) => {
                Some([mode.as_byte(), 0, 0, 0])
            }
            ParsedPacket::ControlCommand { id, data } if *id == self.command().0 => Some(*data),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ChangeError {
    #[error("AirPods did not report the requested setting before the deadline")]
    Timeout,
    #[error("AirPods setting exchange failed: {0}")]
    Transport(#[from] TransportError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOutcome {
    AlreadySet,
    Confirmed,
}

/// Observe the initial setting before writing, then require a matching new
/// notification. Unrelated packets remain available to the caller's state model.
pub fn change_setting(
    transport: &dyn L2capTransport,
    change: SettingChange,
    timeout: Duration,
    mut observe: impl FnMut(ParsedPacket),
) -> Result<ChangeOutcome, ChangeError> {
    let started = Instant::now();
    let mut buf = [0u8; 1024];
    let (id, target) = change.command();
    let mut sent = false;
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(ChangeError::Timeout);
        }
        let size = transport.recv_timeout(&mut buf, remaining)?;
        let packet = match parser::parse(&buf[..size]) {
            Ok(packet) => packet,
            Err(_) => continue,
        };
        let value = change.reported_value(&packet);
        observe(packet);
        if let Some(value) = value {
            if value == target {
                return Ok(if sent {
                    ChangeOutcome::Confirmed
                } else {
                    ChangeOutcome::AlreadySet
                });
            }
            if !sent {
                transport.send(&ControlCommand::create(id.as_byte(), &target))?;
                sent = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Peer {
        incoming: Mutex<VecDeque<Vec<u8>>>,
        sent: Mutex<Vec<Vec<u8>>>,
    }

    impl Peer {
        fn new(packets: Vec<Vec<u8>>) -> Self {
            Self {
                incoming: Mutex::new(packets.into()),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl L2capTransport for Peer {
        fn send(&self, packet: &[u8]) -> Result<(), TransportError> {
            self.sent.lock().expect("test lock").push(packet.to_vec());
            Ok(())
        }

        fn recv(&self, _: &mut [u8]) -> Result<usize, TransportError> {
            panic!("setting changes must use bounded reads")
        }

        fn recv_timeout(&self, buf: &mut [u8], _: Duration) -> Result<usize, TransportError> {
            let packet = self
                .incoming
                .lock()
                .expect("test lock")
                .pop_front()
                .ok_or_else(|| TransportError::new("simulated deadline"))?;
            buf[..packet.len()].copy_from_slice(&packet);
            Ok(packet.len())
        }

        fn close(&self) -> Result<(), TransportError> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
    }

    #[test]
    fn mode_change_requires_a_new_matching_notification_and_preserves_other_events() {
        use crate::protocol::packets;
        let peer = Peer::new(vec![
            packets::build_noise_control(NoiseControlMode::NoiseCancellation).to_vec(),
            ControlCommand::enabled(ControlCommandId::ConversationDetectConfig.as_byte()).to_vec(),
            packets::build_noise_control(NoiseControlMode::NoiseCancellation).to_vec(),
            packets::build_noise_control(NoiseControlMode::Transparency).to_vec(),
        ]);
        let mut observed = 0;
        let outcome = change_setting(
            &peer,
            SettingChange::ListeningMode(NoiseControlMode::Transparency),
            Duration::from_secs(1),
            |_| observed += 1,
        )
        .expect("confirmed");
        assert_eq!(outcome, ChangeOutcome::Confirmed);
        assert_eq!(observed, 4);
        assert_eq!(
            *peer.sent.lock().expect("test lock"),
            vec![packets::build_noise_control(NoiseControlMode::Transparency).to_vec()]
        );
    }

    #[test]
    fn existing_value_succeeds_without_writing() {
        let peer = Peer::new(vec![ControlCommand::enabled(0x28).to_vec()]);
        assert_eq!(
            change_setting(
                &peer,
                SettingChange::ConversationalAwareness(true),
                Duration::from_secs(1),
                |_| {}
            )
            .expect("already enabled"),
            ChangeOutcome::AlreadySet
        );
        assert!(peer.sent.lock().expect("test lock").is_empty());
    }

    #[test]
    fn missing_acknowledgement_is_an_error_and_does_not_repeat_the_mutation() {
        let peer = Peer::new(vec![ControlCommand::disabled(0x1B).to_vec()]);
        assert!(change_setting(
            &peer,
            SettingChange::SingleEarbudAnc(true),
            Duration::from_secs(1),
            |_| {}
        )
        .is_err());
        assert_eq!(
            *peer.sent.lock().expect("test lock"),
            vec![ControlCommand::enabled(0x1B).to_vec()]
        );
    }

    #[test]
    fn no_initial_report_means_no_write() {
        let peer = Peer::new(vec![]);
        assert!(change_setting(
            &peer,
            SettingChange::ConversationalAwareness(false),
            Duration::ZERO,
            |_| {}
        )
        .is_err());
        assert!(peer.sent.lock().expect("test lock").is_empty());
    }
}
