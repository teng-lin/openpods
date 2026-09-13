// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! AAP opcode constants.
//!
//! Every AAP L2CAP packet has the format:
//! ```text
//! 04 00 04 00 [opcode_lo] [opcode_hi] [data...]
//! ```
//! The opcode sits at byte index 4 (little-endian, high byte at index 5 is
//! always 0x00 for known opcodes).
//!
//! Source: `AACPManager.kt` `Opcodes` object, `airpods_packets.h` headers.

/// Battery information notification (opcode `0x04`).
pub const BATTERY_INFO: u8 = 0x04;

/// Ear detection status (opcode `0x06`).
pub const EAR_DETECTION: u8 = 0x06;

/// Control command — covers noise control, conversational awareness toggle,
/// long-press config, chime volume, etc. (opcode `0x09`).
/// See [`super::control_command`] for identifiers.
pub const CONTROL_COMMAND: u8 = 0x09;

/// Request notifications from AirPods (opcode `0x0F`).
///
/// Outbound — not part of the receive dispatch.
pub const REQUEST_NOTIFICATIONS: u8 = 0x0F;

/// Head tracking data (opcode `0x17`).
pub const HEADTRACKING: u8 = 0x17;

/// Device metadata — name, model number, manufacturer (opcode `0x1D`).
pub const DEVICE_METADATA: u8 = 0x1D;

/// Rename device (opcode `0x1A`).
///
/// Outbound — not part of the receive dispatch.
pub const RENAME: u8 = 0x1A;

/// Proximity keys request (opcode `0x30`).
pub const PROXIMITY_KEYS_REQ: u8 = 0x30;

/// Proximity keys response (opcode `0x31`).
pub const PROXIMITY_KEYS_RSP: u8 = 0x31;

/// Conversational awareness data (opcode `0x4B`).
pub const CONVERSATION_AWARENESS: u8 = 0x4B;

/// Set feature flags — enables Conversational Awareness during audio playback
/// and Adaptive Transparency (opcode `0x4D`).
///
/// Outbound — not part of the receive dispatch.
pub const SET_FEATURE_FLAGS: u8 = 0x4D;

// ---------------------------------------------------------------------------
// Additional opcodes (documented upstream; not yet dispatched)
//
// Sourced from `AACPManager.kt` `Opcodes`. They are not yet matched in
// `parser::parse()` and therefore currently fall through to
// `ParsedPacket::Unknown`. Kept here as the single source of truth for future
// dispatch work.
// ---------------------------------------------------------------------------

/// Audio source notification (opcode `0x0E`).
pub const AUDIO_SOURCE: u8 = 0x0E;

/// Smart routing request (opcode `0x10`).
pub const SMART_ROUTING: u8 = 0x10;

/// Smart routing response (opcode `0x11`).
pub const SMART_ROUTING_RESP: u8 = 0x11;

/// Send connected device MAC (opcode `0x14`).
pub const SEND_CONNECTED_MAC: u8 = 0x14;

/// Stem press gesture (opcode `0x19`).
pub const STEM_PRESS: u8 = 0x19;

/// Connected devices response (opcode `0x2E`).
pub const CONNECTED_DEVICES: u8 = 0x2E;

/// Headphone accommodation / EQ response (opcode `0x53`).
pub const HEADPHONE_ACCOMMODATION: u8 = 0x53;

/// Custom EQ (opcode `0x63`).
pub const CUSTOM_EQ: u8 = 0x63;
