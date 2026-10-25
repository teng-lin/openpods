// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Persistent session ownership, local control API and desktop orchestration.

#[cfg(target_os = "linux")]
pub mod backend;
#[cfg(target_os = "linux")]
pub mod ble;
pub use librepods_api::config;
pub use librepods_api::error;
pub use librepods_api::ipc;
#[cfg(target_os = "linux")]
pub mod keys;
#[cfg(target_os = "linux")]
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod session;
pub use librepods_api::snapshot;
#[cfg(target_os = "linux")]
pub mod storage;

pub use error::{DaemonError, ErrorCode};

#[cfg(target_os = "linux")]
pub mod desktop;

#[cfg(target_os = "linux")]
mod telemetry;
