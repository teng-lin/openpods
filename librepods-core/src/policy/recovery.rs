// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Recovery budgets and scheduling with caller-supplied monotonic time and jitter.

use serde::{Deserialize, Serialize};

pub const FIRST_RETRY_DELAY_MS: u64 = 750;
pub const MAX_RETRY_DELAY_MS: u64 = 16_000;
pub const MAX_CONNECTED_ATTEMPTS: u32 = 10;
pub const DEFAULT_ABSENT_ATTEMPTS: u32 = 3;
pub const RECONCILE_INTERVAL_MS: u64 = 30_000;
pub const MAX_JITTER_MS: u64 = 499;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    #[default]
    WaitingForAdapter,
    WaitingForDevice,
    Connecting,
    AwaitingHandshake,
    AwaitingFeatures,
    Subscribing,
    Ready,
    Recovering,
    Suspended,
    Stopping,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    Wait,
    RetryAt(u64),
    Reconcile,
    Stop,
}

pub struct RecoveryPolicy {
    pub state: Lifecycle,
    pub due_ms: Option<u64>,
    pub generation: u64,
    pub connected_attempts: u32,
    pub absent_attempts: u32,
    max_absent_attempts: u32,
    desired: bool,
    watchdog_at_ms: u64,
}
impl RecoveryPolicy {
    pub fn new(max_absent_attempts: u32) -> Self {
        Self {
            state: Lifecycle::WaitingForAdapter,
            due_ms: None,
            generation: 0,
            connected_attempts: 0,
            absent_attempts: 0,
            max_absent_attempts,
            desired: true,
            watchdog_at_ms: 0,
        }
    }
    pub fn ready(&mut self, now_ms: u64) {
        self.state = Lifecycle::Ready;
        self.due_ms = None;
        self.connected_attempts = 0;
        self.absent_attempts = 0;
        self.watchdog_at_ms = now_ms.saturating_add(RECONCILE_INTERVAL_MS);
    }
    pub fn on_failure(
        &mut self,
        now_ms: u64,
        bluez_connected: bool,
        jitter_ms: u64,
    ) -> RecoveryDecision {
        if !self.desired || matches!(self.state, Lifecycle::Suspended | Lifecycle::Stopping) {
            return RecoveryDecision::Wait;
        }
        self.generation = self.generation.saturating_add(1);
        self.state = Lifecycle::Recovering;
        let attempt = if bluez_connected {
            if self.connected_attempts >= MAX_CONNECTED_ATTEMPTS {
                self.due_ms = None;
                self.watchdog_at_ms = now_ms.saturating_add(RECONCILE_INTERVAL_MS);
                return RecoveryDecision::Wait;
            }
            self.connected_attempts += 1;
            self.connected_attempts
        } else {
            if self.absent_attempts >= self.max_absent_attempts {
                self.due_ms = None;
                self.state = Lifecycle::WaitingForDevice;
                return RecoveryDecision::Wait;
            }
            self.absent_attempts += 1;
            self.absent_attempts
        };
        let delay = if attempt == 1 {
            FIRST_RETRY_DELAY_MS
        } else {
            1000_u64
                .saturating_mul(1 << ((attempt - 2).min(4)))
                .min(MAX_RETRY_DELAY_MS)
        };
        let due = now_ms
            .saturating_add(delay)
            .saturating_add(jitter_ms.min(MAX_JITTER_MS));
        self.due_ms = Some(due);
        RecoveryDecision::RetryAt(due)
    }
    pub fn tick(
        &mut self,
        now_ms: u64,
        adapter_ready: bool,
        bluez_connected: bool,
        have_address: bool,
    ) -> RecoveryDecision {
        if !self.desired
            || matches!(
                self.state,
                Lifecycle::Ready | Lifecycle::Suspended | Lifecycle::Stopping
            )
        {
            return RecoveryDecision::Wait;
        }
        if !adapter_ready {
            self.state = Lifecycle::WaitingForAdapter;
            return RecoveryDecision::Wait;
        }
        if self.due_ms.is_some_and(|due| now_ms >= due) {
            self.due_ms = None;
            self.state = Lifecycle::Connecting;
            return RecoveryDecision::Reconcile;
        }
        if self.due_ms.is_none()
            && now_ms >= self.watchdog_at_ms
            && (bluez_connected || !have_address)
        {
            self.watchdog_at_ms = now_ms.saturating_add(RECONCILE_INTERVAL_MS);
            self.connected_attempts = 0;
            return RecoveryDecision::Reconcile;
        }
        RecoveryDecision::Wait
    }
    pub fn suspend(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.state = Lifecycle::Suspended;
        self.due_ms = None;
    }
    pub fn resume(&mut self, now_ms: u64) -> RecoveryDecision {
        if self.state != Lifecycle::Suspended || !self.desired {
            return RecoveryDecision::Wait;
        }
        self.state = Lifecycle::WaitingForAdapter;
        self.connected_attempts = 0;
        self.absent_attempts = 0;
        self.due_ms = Some(now_ms);
        self.watchdog_at_ms = now_ms;
        RecoveryDecision::Reconcile
    }
    pub fn disconnect_intentionally(&mut self) {
        self.desired = false;
        self.due_ms = None;
        self.generation = self.generation.saturating_add(1);
        self.state = Lifecycle::WaitingForDevice;
    }
    pub fn reconnect_explicitly(&mut self, now_ms: u64) -> RecoveryDecision {
        self.desired = true;
        self.connected_attempts = 0;
        self.absent_attempts = 0;
        self.due_ms = Some(now_ms);
        self.watchdog_at_ms = now_ms;
        self.state = Lifecycle::WaitingForAdapter;
        RecoveryDecision::Reconcile
    }
    pub fn stop(&mut self) -> RecoveryDecision {
        self.desired = false;
        self.state = Lifecycle::Stopping;
        self.due_ms = None;
        self.generation = self.generation.saturating_add(1);
        RecoveryDecision::Stop
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_budgets_cap_backoff_and_watchdog_only_reclaims_connected_device() {
        let mut policy = RecoveryPolicy::new(2);
        assert_eq!(
            policy.on_failure(0, false, 0),
            RecoveryDecision::RetryAt(750)
        );
        assert_eq!(policy.tick(749, true, false, true), RecoveryDecision::Wait);
        assert_eq!(
            policy.tick(750, true, false, true),
            RecoveryDecision::Reconcile
        );
        assert_eq!(
            policy.on_failure(1000, false, 999),
            RecoveryDecision::RetryAt(2499)
        );
        assert_eq!(policy.on_failure(2000, false, 0), RecoveryDecision::Wait);
        assert_eq!(
            policy.tick(99_000, true, false, true),
            RecoveryDecision::Wait
        );
        for _ in 0..10 {
            assert!(matches!(
                policy.on_failure(100_000, true, 0),
                RecoveryDecision::RetryAt(_)
            ));
        }
        assert_eq!(policy.due_ms, Some(116_000));
        assert_eq!(policy.on_failure(100_000, true, 0), RecoveryDecision::Wait);
        assert_eq!(
            policy.tick(129_999, true, true, true),
            RecoveryDecision::Wait
        );
        assert_eq!(
            policy.tick(130_000, true, true, true),
            RecoveryDecision::Reconcile
        );
    }
    #[test]
    fn suspend_manual_disconnect_and_stop_cancel_timers() {
        let mut policy = RecoveryPolicy::new(3);
        policy.on_failure(0, true, 0);
        policy.suspend();
        assert_eq!(policy.on_failure(1, true, 0), RecoveryDecision::Wait);
        assert_eq!(policy.tick(1000, true, true, true), RecoveryDecision::Wait);
        assert_eq!(policy.resume(1001), RecoveryDecision::Reconcile);
        assert_eq!(policy.resume(1002), RecoveryDecision::Wait);
        policy.disconnect_intentionally();
        assert_eq!(
            policy.tick(100_000, true, true, true),
            RecoveryDecision::Wait
        );
        assert_eq!(
            policy.reconnect_explicitly(100_001),
            RecoveryDecision::Reconcile
        );
        policy.ready(100_002);
        assert_eq!(
            policy.tick(200_000, true, true, true),
            RecoveryDecision::Wait
        );
        assert_eq!(policy.stop(), RecoveryDecision::Stop);
        assert_eq!(policy.on_failure(200_001, true, 0), RecoveryDecision::Wait);
    }
    #[test]
    fn explicit_reconnect_and_resume_replace_old_timers_and_reset_budgets() {
        let mut policy = RecoveryPolicy::new(DEFAULT_ABSENT_ATTEMPTS);
        policy.on_failure(10_000, true, 0);
        assert_eq!(
            policy.reconnect_explicitly(10_100),
            RecoveryDecision::Reconcile
        );
        assert_eq!(policy.due_ms, Some(10_100));
        assert_eq!(
            policy.tick(10_100, false, true, true),
            RecoveryDecision::Wait
        );
        assert_eq!(
            policy.tick(10_101, true, true, true),
            RecoveryDecision::Reconcile
        );
        policy.on_failure(10_102, true, 0);
        policy.on_failure(10_103, false, 0);
        policy.suspend();
        assert_eq!(policy.resume(10_104), RecoveryDecision::Reconcile);
        assert_eq!((policy.connected_attempts, policy.absent_attempts), (0, 0));
        assert_eq!(
            policy.tick(10_104, true, true, true),
            RecoveryDecision::Reconcile
        );
    }
    #[test]
    fn cold_start_without_adapter_waits_then_reconciles_without_an_address() {
        let mut policy = RecoveryPolicy::new(0);
        assert_eq!(policy.tick(0, false, false, false), RecoveryDecision::Wait);
        assert_eq!(
            policy.tick(1, true, false, false),
            RecoveryDecision::Reconcile
        );
        assert_eq!(policy.on_failure(2, false, 0), RecoveryDecision::Wait);
    }
}
