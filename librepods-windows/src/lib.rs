// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Windows platform implementations for LibrePods transport traits.
//!
//! This crate is only compiled on Windows. It provides:
//! - [`l2cap`]: raw Bluetooth L2CAP sockets via WinSock (`AF_BTH`)
//! - [`device_monitor`]: AirPods connection events via WinRT Bluetooth APIs
//! - [`ble`]: BLE advertisement scanning via `btleplug`
//! - [`media`]: playback via SMTC (WinRT) and default-render checks

#![cfg(target_os = "windows")]

pub mod ble;
pub mod device_monitor;
pub mod l2cap;
pub mod media;
