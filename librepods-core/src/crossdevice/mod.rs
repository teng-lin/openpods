// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Cross-device sync protocol.
//!
//! # Status: experimental, not wired up
//!
//! This module is **not** used by `librepods-cli` or either platform crate yet;
//! only its own unit tests exercise it. Several packet constants
//! ([`packets::REQUEST_BATTERY`], [`packets::REQUEST_ANC`],
//! [`packets::DATA_HEADER`]) cannot be verified against the current upstream
//! reference, and the upstream sources cited below (`CrossDevice.kt`,
//! `main.cpp`) are no longer present in that tree. Treat this as a
//! work-in-progress design and do not rely on it.
//!
//! References (upstream; may not exist in the current tree):
//! - `CrossDevice.kt` `handleClientConnection()`
//! - `main.cpp` `handlePhonePacket()`

pub mod packets;
pub mod relay;
