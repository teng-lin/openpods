// SPDX-License-Identifier: AGPL-3.0-only
//! Testable desktop state and action policy. Bluetooth belongs exclusively to the daemon.
#[cfg(target_os = "linux")]
pub mod client;
pub mod demo;
pub mod model;
