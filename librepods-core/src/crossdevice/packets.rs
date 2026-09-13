// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Cross-device packet constants.
//!
//! References:
//! - `CrossDevice.kt` `CrossDevicePackets` enum
//! - `airpods_packets.h` `Phone::*` namespace

/// RFCOMM service UUID for cross-device communication.
pub const RFCOMM_UUID: &str = "1abbb9a4-10e4-4000-a75c-8953c5471342";

/// BLE manufacturer ID used for cross-device discovery.
pub const MANUFACTURER_ID: u16 = 0x1234;

/// BLE manufacturer data string for cross-device discovery.
pub const MANUFACTURER_DATA: &[u8] = b"ALN_AirPods";

// ---------------------------------------------------------------------------
// Packet constants (4-byte fixed-size)
// ---------------------------------------------------------------------------

/// AirPods are connected on the remote device.
pub const CONNECTED: [u8; 4] = [0x00, 0x01, 0x00, 0x01];

/// AirPods are disconnected on the remote device.
pub const DISCONNECTED: [u8; 4] = [0x00, 0x01, 0x00, 0x00];

/// Request the remote device to disconnect its AirPods.
pub const REQUEST_DISCONNECT: [u8; 4] = [0x00, 0x02, 0x00, 0x00];

/// Request battery data from the remote device.
pub const REQUEST_BATTERY: [u8; 4] = [0x00, 0x02, 0x00, 0x01];

/// Request ANC state from the remote device.
pub const REQUEST_ANC: [u8; 4] = [0x00, 0x02, 0x00, 0x02];

/// Request connection status from the remote device.
pub const REQUEST_STATUS: [u8; 4] = [0x00, 0x02, 0x00, 0x03];

/// Header prefix for relayed AirPods data packets.
///
/// NOTE: this shares the wire value `00 04 00 01` with [`NOTIFICATION`]. The
/// upstream reference (`airpods_packets.h`) defines only `Phone::NOTIFICATION`
/// under that value and no separate relay header, so the two names currently
/// describe the same bytes. Revisit if a distinct relay header is identified.
pub const DATA_HEADER: [u8; 4] = [0x00, 0x04, 0x00, 0x01];

/// Notification packet sent to phone when AirPods connect on Linux side.
///
/// Wire value is identical to [`DATA_HEADER`] — see its note.
pub const NOTIFICATION: [u8; 4] = [0x00, 0x04, 0x00, 0x01];
