// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Wear transitions with a fixed both-out settlement window.

use crate::device::state::WearReport;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EarMode {
    #[default]
    Off,
    OneRemoved,
    BothRemoved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarAction {
    None,
    Pause,
    Resume,
}

pub struct EarPolicy {
    mode: EarMode,
    settle_ms: u64,
    generation: Option<u64>,
    count: Option<u8>,
    pending_out: Option<u64>,
}

impl EarPolicy {
    pub fn new(mode: EarMode, settle_ms: u64) -> Self {
        Self {
            mode,
            settle_ms,
            generation: None,
            count: None,
            pending_out: None,
        }
    }
    pub fn reset(&mut self) {
        self.generation = None;
        self.count = None;
        self.pending_out = None;
    }
    pub fn set_mode(&mut self, mode: EarMode) {
        self.mode = mode;
        self.reset();
    }

    pub fn on_wear(
        &mut self,
        report: WearReport,
        generation: u64,
        now_ms: u64,
        active_output: bool,
        owns_pause: bool,
    ) -> EarAction {
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
        let (Some(primary), Some(secondary)) = (report.primary.in_ear(), report.secondary.in_ear())
        else {
            self.count = None;
            self.pending_out = None;
            return EarAction::None;
        };
        let count = u8::from(primary) + u8::from(secondary);
        let previous = self.count.replace(count);
        let Some(previous) = previous else {
            return EarAction::None;
        };
        if self.mode == EarMode::Off || !active_output {
            self.pending_out = None;
            return EarAction::None;
        }
        if count == 0 {
            if previous > 0 && !owns_pause && self.pending_out.is_none() {
                self.pending_out = Some(now_ms.saturating_add(self.settle_ms));
            }
            return self.tick(generation, now_ms, active_output, owns_pause);
        }
        self.pending_out = None;
        let pause = self.mode == EarMode::OneRemoved && previous == 2 && count < 2;
        let resume = match self.mode {
            EarMode::OneRemoved => previous < 2 && count == 2,
            EarMode::BothRemoved => previous == 0 && count > 0,
            EarMode::Off => false,
        };
        if pause && !owns_pause {
            EarAction::Pause
        } else if resume && owns_pause {
            EarAction::Resume
        } else {
            EarAction::None
        }
    }

    pub fn tick(
        &mut self,
        generation: u64,
        now_ms: u64,
        active_output: bool,
        owns_pause: bool,
    ) -> EarAction {
        if self.generation != Some(generation) {
            self.reset();
            return EarAction::None;
        }
        if !active_output || self.mode == EarMode::Off {
            self.pending_out = None;
            return EarAction::None;
        }
        if self.pending_out.is_some_and(|deadline| now_ms >= deadline) {
            self.pending_out = None;
            if self.count == Some(0) && !owns_pause {
                return EarAction::Pause;
            }
        }
        EarAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::state::WearState::{InEar, OutOfEar, Unknown};
    fn report(primary: bool, secondary: bool) -> WearReport {
        WearReport {
            primary: if primary { InEar } else { OutOfEar },
            secondary: if secondary { InEar } else { OutOfEar },
        }
    }
    #[test]
    fn baseline_and_transient_reports_never_start_playback_or_extend_deadline() {
        let mut policy = EarPolicy::new(EarMode::BothRemoved, 500);
        assert_eq!(
            policy.on_wear(report(true, true), 1, 0, true, false),
            EarAction::None
        );
        assert_eq!(
            policy.on_wear(report(false, false), 1, 1, true, false),
            EarAction::None
        );
        assert_eq!(
            policy.on_wear(report(false, false), 1, 499, true, false),
            EarAction::None
        );
        assert_eq!(policy.tick(1, 501, true, false), EarAction::Pause);
        assert_eq!(policy.tick(1, 502, true, false), EarAction::None);
        assert_eq!(
            policy.on_wear(report(true, false), 1, 503, true, true),
            EarAction::Resume
        );
    }
    #[test]
    fn in_ear_and_new_policy_cancel_pending_actions() {
        let mut policy = EarPolicy::new(EarMode::BothRemoved, 500);
        policy.on_wear(report(true, true), 1, 0, true, false);
        policy.on_wear(report(false, false), 1, 1, true, false);
        policy.on_wear(report(true, true), 1, 2, true, false);
        assert_eq!(policy.tick(1, 1000, true, false), EarAction::None);
        policy.on_wear(report(false, false), 1, 1001, true, false);
        policy.set_mode(EarMode::Off);
        assert_eq!(policy.tick(1, 2000, true, false), EarAction::None);
        assert_eq!(
            policy.on_wear(report(true, true), 1, 2001, true, true),
            EarAction::None
        );
    }
    #[test]
    fn one_removed_unknown_wear_output_and_generation_are_conservative() {
        let mut policy = EarPolicy::new(EarMode::OneRemoved, 500);
        policy.on_wear(report(true, true), 1, 0, true, false);
        assert_eq!(
            policy.on_wear(report(true, false), 1, 1, true, false),
            EarAction::Pause
        );
        assert_eq!(
            policy.on_wear(report(true, true), 1, 2, true, false),
            EarAction::None
        );
        policy.on_wear(report(true, false), 1, 3, true, true);
        assert_eq!(
            policy.on_wear(report(true, true), 1, 4, true, true),
            EarAction::Resume
        );
        policy.on_wear(
            WearReport {
                primary: Unknown(9),
                secondary: InEar,
            },
            1,
            5,
            true,
            true,
        );
        assert_eq!(
            policy.on_wear(report(true, true), 1, 6, true, true),
            EarAction::None
        );
        assert_eq!(
            policy.on_wear(report(false, false), 1, 7, false, false),
            EarAction::None
        );
        assert_eq!(policy.tick(1, 1000, true, false), EarAction::None);
        policy.on_wear(report(true, true), 1, 1001, true, false);
        policy.on_wear(report(false, false), 1, 1002, true, false);
        assert_eq!(policy.tick(2, 2000, true, false), EarAction::None);
    }
}
