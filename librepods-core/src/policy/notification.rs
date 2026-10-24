// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Per-component low-battery hysteresis, independent of notification delivery.

pub struct LowBatteryLatch {
    trigger: u8,
    reset: u8,
    notified: bool,
}
impl LowBatteryLatch {
    pub fn new(trigger: u8, reset: u8) -> Self {
        Self {
            trigger: trigger.min(100),
            reset: reset.min(100).max(trigger.min(100)),
            notified: false,
        }
    }
    pub fn reset(&mut self) {
        self.notified = false;
    }
    pub fn evaluate(&mut self, level: Option<u8>, charging: bool, fresh: bool) -> Option<u8> {
        let level = level.filter(|level| *level <= 100)?;
        if !fresh {
            return None;
        }
        if charging || level >= self.reset {
            self.reset();
        }
        if !charging && level <= self.trigger && !self.notified {
            self.notified = true;
            Some(level)
        } else {
            None
        }
    }
}
impl Default for LowBatteryLatch {
    fn default() -> Self {
        Self::new(10, 15)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hysteresis_ignores_unknown_stale_and_invalid_readings() {
        let mut latch = LowBatteryLatch::default();
        assert_eq!(latch.evaluate(None, false, true), None);
        assert_eq!(latch.evaluate(Some(101), false, true), None);
        assert_eq!(latch.evaluate(Some(0), false, false), None);
        assert_eq!(latch.evaluate(Some(10), false, true), Some(10));
        assert_eq!(latch.evaluate(Some(11), false, true), None);
        assert_eq!(latch.evaluate(Some(9), false, true), None);
        assert_eq!(latch.evaluate(Some(15), false, true), None);
        assert_eq!(latch.evaluate(Some(0), false, true), Some(0));
        assert_eq!(latch.evaluate(Some(0), true, true), None);
        assert_eq!(latch.evaluate(Some(0), false, true), Some(0));
        latch.reset();
        assert_eq!(latch.evaluate(Some(9), false, true), Some(9));
    }
}
