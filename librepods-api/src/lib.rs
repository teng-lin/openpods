// SPDX-License-Identifier: AGPL-3.0-only
//! Shared local protocol and status DTOs. No Bluetooth, D-Bus, async runtime or socket ownership.
pub mod config;
pub mod error;
pub mod ipc;
pub mod snapshot;
pub use error::{DaemonError, ErrorCode};

#[cfg(all(target_os = "linux", feature = "transport"))]
pub mod transport;
