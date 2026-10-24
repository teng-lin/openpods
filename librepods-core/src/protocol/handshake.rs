// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Pure handshake progression. The owner performs returned writes.

use super::{packets, parser::ParsedPacket};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeStage {
    New,
    Handshake,
    Features,
    Subscription,
    Ready,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeAction {
    Wait,
    Send(Vec<u8>),
    Ready,
    TimedOut,
    Cancelled,
}

pub struct Handshake {
    pub generation: u64,
    pub stage: HandshakeStage,
    pub deadline_ms: u64,
    stage_budget_ms: u64,
}

impl Handshake {
    pub fn new(generation: u64, now_ms: u64, stage_budget_ms: u64) -> Self {
        Self {
            generation,
            stage: HandshakeStage::New,
            deadline_ms: now_ms.saturating_add(stage_budget_ms),
            stage_budget_ms,
        }
    }

    pub fn start(&mut self) -> HandshakeAction {
        if self.stage != HandshakeStage::New {
            return HandshakeAction::Wait;
        }
        self.stage = HandshakeStage::Handshake;
        HandshakeAction::Send(packets::HANDSHAKE.to_vec())
    }

    pub fn tick(&mut self, generation: u64, now_ms: u64) -> HandshakeAction {
        match self.stage {
            HandshakeStage::Ready => return HandshakeAction::Ready,
            HandshakeStage::TimedOut => return HandshakeAction::TimedOut,
            HandshakeStage::Cancelled => return HandshakeAction::Cancelled,
            _ => {}
        }
        if generation != self.generation {
            self.stage = HandshakeStage::Cancelled;
            return HandshakeAction::Cancelled;
        }
        if now_ms >= self.deadline_ms {
            self.stage = HandshakeStage::TimedOut;
            return HandshakeAction::TimedOut;
        }
        HandshakeAction::Wait
    }

    pub fn on_packet(
        &mut self,
        packet: &ParsedPacket,
        generation: u64,
        now_ms: u64,
    ) -> HandshakeAction {
        let action = self.tick(generation, now_ms);
        if action != HandshakeAction::Wait {
            return action;
        }
        let next = match (self.stage, packet) {
            (HandshakeStage::Handshake, ParsedPacket::HandshakeAck) => {
                self.stage = HandshakeStage::Features;
                Some(packets::SET_SPECIFIC_FEATURES.to_vec())
            }
            (HandshakeStage::Features, ParsedPacket::FeaturesAck) => {
                self.stage = HandshakeStage::Subscription;
                Some(packets::REQUEST_NOTIFICATIONS.to_vec())
            }
            _ => None,
        };
        if let Some(packet) = next {
            self.deadline_ms = now_ms.saturating_add(self.stage_budget_ms);
            HandshakeAction::Send(packet)
        } else {
            HandshakeAction::Wait
        }
    }

    pub fn mark_subscribed(&mut self, generation: u64, now_ms: u64) -> HandshakeAction {
        let action = self.tick(generation, now_ms);
        if action != HandshakeAction::Wait {
            return action;
        }
        if self.stage == HandshakeStage::Subscription {
            self.stage = HandshakeStage::Ready;
            HandshakeAction::Ready
        } else {
            HandshakeAction::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sequencing_requires_expected_ack_and_successful_subscription_write() {
        let mut machine = Handshake::new(3, 0, 100);
        assert_eq!(
            machine.start(),
            HandshakeAction::Send(packets::HANDSHAKE.to_vec())
        );
        assert_eq!(machine.start(), HandshakeAction::Wait);
        assert_eq!(machine.mark_subscribed(3, 1), HandshakeAction::Wait);
        assert_eq!(
            machine.on_packet(&ParsedPacket::FeaturesAck, 3, 2),
            HandshakeAction::Wait
        );
        assert_eq!(
            machine.on_packet(&ParsedPacket::HandshakeAck, 3, 90),
            HandshakeAction::Send(packets::SET_SPECIFIC_FEATURES.to_vec())
        );
        assert_eq!(machine.deadline_ms, 190);
        assert_eq!(
            machine.on_packet(&ParsedPacket::FeaturesAck, 3, 180),
            HandshakeAction::Send(packets::REQUEST_NOTIFICATIONS.to_vec())
        );
        assert_eq!(machine.stage, HandshakeStage::Subscription);
        assert_eq!(machine.mark_subscribed(3, 181), HandshakeAction::Ready);
        assert_eq!(machine.tick(3, 500), HandshakeAction::Ready);
    }
    #[test]
    fn unrelated_frames_do_not_extend_deadlines_and_stale_generations_cancel() {
        let mut machine = Handshake::new(1, 0, 100);
        machine.start();
        assert_eq!(
            machine.on_packet(&ParsedPacket::Unknown(vec![]), 1, 99),
            HandshakeAction::Wait
        );
        assert_eq!(machine.tick(1, 100), HandshakeAction::TimedOut);
        assert_eq!(machine.mark_subscribed(1, 101), HandshakeAction::TimedOut);
        let mut machine = Handshake::new(1, 0, 100);
        assert_eq!(machine.tick(2, 1), HandshakeAction::Cancelled);
        assert_eq!(machine.tick(1, 1), HandshakeAction::Cancelled);
    }
}
