// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Persistent session ownership, local control API and desktop orchestration.

#[cfg(target_os = "linux")]
pub mod backend;
pub mod config;
pub mod error;
pub mod ipc;
#[cfg(target_os = "linux")]
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod session;
pub mod snapshot;
#[cfg(target_os = "linux")]
pub mod storage;

pub use error::{DaemonError, ErrorCode};
