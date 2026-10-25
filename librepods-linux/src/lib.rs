// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Linux platform implementations for LibrePods transport traits.
//!
//! This crate is only compiled on Linux. On other targets it is empty so that
//! a workspace-wide `cargo build` still succeeds.

#![cfg(target_os = "linux")]

pub mod ble;
pub mod bluez_monitor;
pub mod desktop;
pub mod device;
pub mod error;
pub mod l2cap;
pub mod media;
pub mod sleep;

#[cfg(test)]
mod test_bus;

mod player_monitor;
