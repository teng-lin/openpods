// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Incoming AAP packet parser.
//!
//! Dispatches raw bytes received from the L2CAP socket into typed
//! [`ParsedPacket`] variants by matching on the opcode byte at index 4.
//!
//! References:
//! - `AACPManager.kt` `receivePacket()`
//! - `main.cpp` `parseData()`

use crate::device::battery::{self, BatteryInfo};
use crate::device::state::{NoiseControlMode, WearState};
use crate::protocol::control_command::ControlCommandId;
use crate::protocol::opcodes;
use crate::protocol::packets;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed packet: {0}")]
    Malformed(&'static str),
    #[error("packet too short ({0} bytes)")]
    TooShort(usize),

    #[error("missing AAP header")]
    BadHeader,

    #[error("unknown opcode 0x{0:02X}")]
    UnknownOpcode(u8),

    #[error("battery parse error: {0}")]
    Battery(#[from] battery::BatteryParseError),

    #[error("invalid metadata: {0}")]
    Metadata(String),

    #[error("invalid magic cloud keys: {0}")]
    MagicCloudKeys(String),
}

// ---------------------------------------------------------------------------
// Proximity key types
// ---------------------------------------------------------------------------

/// Proximity key type in a keys-response TLV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ProximityKeyType {
    Irk = 0x01,
    EncKey = 0x04,
}

impl ProximityKeyType {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Irk),
            0x04 => Some(Self::EncKey),
            _ => None,
        }
    }
}

/// A single proximity key extracted from a keys-response packet.
#[derive(Debug, Clone)]
pub struct ProximityKey {
    pub key_type: ProximityKeyType,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Parsed packet enum
// ---------------------------------------------------------------------------

/// Result of parsing an incoming AAP packet.
#[derive(Debug)]
pub enum ParsedPacket {
    /// Battery status for left, right, case.
    Battery(BatteryInfo),

    /// Ear detection: `true` = in ear, `false` = out / in case.
    EarDetection {
        primary_in_ear: bool,
        secondary_in_ear: bool,
    },
    /// Full protocol wear states; unlike legacy booleans this preserves in-case/unknown.
    EarStatus {
        primary: WearState,
        secondary: WearState,
    },

    /// Noise control mode notification from AirPods (stem press or remote set).
    NoiseControl(NoiseControlMode),

    /// Conversational awareness data byte.
    ConversationalAwareness { status: u8 },

    /// A control command received from AirPods.
    ControlCommand { id: ControlCommandId, data: [u8; 4] },

    /// Device metadata (name, model number, manufacturer).
    Metadata {
        name: String,
        model_number: String,
        manufacturer: String,
    },

    /// Head tracking sensor data (the full raw packet, including its 6-byte header).
    HeadTracking(Vec<u8>),

    /// Proximity keys response.
    ProximityKeys(Vec<ProximityKey>),

    /// Handshake ACK — signals that SET_SPECIFIC_FEATURES should be sent.
    HandshakeAck,

    /// Features ACK — signals that REQUEST_NOTIFICATIONS should be sent.
    FeaturesAck,

    /// Magic cloud keys (IRK + EncKey, 16 bytes each).
    MagicCloudKeys { irk: [u8; 16], enc_key: [u8; 16] },

    /// Unrecognised packet.
    Unknown(Vec<u8>),
}

// ---------------------------------------------------------------------------
// Top-level parse function
// ---------------------------------------------------------------------------

/// Parse a raw AAP packet received from the L2CAP socket.
pub fn parse(packet: &[u8]) -> Result<ParsedPacket, ParseError> {
    if packet.len() > packets::MAX_PACKET_BYTES {
        return Err(ParseError::Malformed("datagram exceeds control limit"));
    }
    if packet.len() < 4 {
        return Err(ParseError::TooShort(packet.len()));
    }

    // Handshake ACK has a different header prefix
    if packet.starts_with(&packets::HANDSHAKE_ACK_PREFIX) {
        return Ok(ParsedPacket::HandshakeAck);
    }

    // Features ACK
    if packet.starts_with(&packets::FEATURES_ACK_PREFIX) {
        return Ok(ParsedPacket::FeaturesAck);
    }

    // Magic cloud keys response — the common 2-key proximity response carrying
    // IRK (0x01) + EncKey (0x04). Fall back to the generic proximity parser when
    // the payload does not match that exact layout.
    if packet.starts_with(&packets::MAGIC_CLOUD_KEYS_HEADER) {
        return parse_magic_cloud_keys(packet).or_else(|_| parse_proximity_keys(packet));
    }

    // Standard packets must start with the AAP header
    if packet[..4] != packets::HEADER {
        return Err(ParseError::BadHeader);
    }

    if packet.len() < 6 {
        return Err(ParseError::TooShort(packet.len()));
    }
    if packet[5] != 0 {
        return Err(ParseError::Malformed("nonzero standard reserved byte"));
    }

    let opcode = packet[4];

    match opcode {
        opcodes::BATTERY_INFO => {
            let info = battery::parse_battery(packet)?;
            Ok(ParsedPacket::Battery(info))
        }

        opcodes::EAR_DETECTION => {
            if packet.len() != 8 {
                return Err(ParseError::Malformed(
                    "wear notification must be eight bytes",
                ));
            }
            Ok(ParsedPacket::EarStatus {
                primary: WearState::from_byte(packet[6]),
                secondary: WearState::from_byte(packet[7]),
            })
        }

        opcodes::CONTROL_COMMAND => parse_control_command(packet),

        opcodes::CONVERSATION_AWARENESS => {
            // CA status data: 04 00 04 00 4B 00 02 00 XX YY
            // The status/toggle notification also uses opcode 0x4B.
            if packet.len() >= 10 && packet.starts_with(&packets::CA_DATA_PREFIX) {
                Ok(ParsedPacket::ConversationalAwareness { status: packet[9] })
            } else {
                // It might be a control command-style CA packet (via opcode 0x09
                // with id 0x28). Fall through to Unknown.
                Ok(ParsedPacket::Unknown(packet.to_vec()))
            }
        }

        opcodes::DEVICE_METADATA => parse_metadata(packet),

        opcodes::HEADTRACKING => {
            if packet.len() < 70 {
                return Err(ParseError::TooShort(packet.len()));
            }
            Ok(ParsedPacket::HeadTracking(packet.to_vec()))
        }

        opcodes::PROXIMITY_KEYS_RSP => parse_proximity_keys(packet),

        _ => Ok(ParsedPacket::Unknown(packet.to_vec())),
    }
}

// ---------------------------------------------------------------------------
// Sub-parsers
// ---------------------------------------------------------------------------

/// Parse a control command notification from AirPods.
fn parse_control_command(packet: &[u8]) -> Result<ParsedPacket, ParseError> {
    if packet.len() != 11 {
        return Err(ParseError::Malformed(
            "control notification must be eleven bytes",
        ));
    }

    let id_byte = packet[6];
    let id = match ControlCommandId::from_byte(id_byte) {
        Some(id) => id,
        None => {
            return Ok(ParsedPacket::Unknown(packet.to_vec()));
        }
    };

    // If it's a noise control mode change, also return a typed variant
    if id == ControlCommandId::ListeningMode {
        if packet[8..] != [0, 0, 0] {
            return Ok(ParsedPacket::Unknown(packet.to_vec()));
        }
        let mode = match NoiseControlMode::from_byte(packet[7]) {
            Some(m) => m,
            None => return Ok(ParsedPacket::Unknown(packet.to_vec())),
        };
        return Ok(ParsedPacket::NoiseControl(mode));
    }

    let data = [packet[7], packet[8], packet[9], packet[10]];
    Ok(ParsedPacket::ControlCommand { id, data })
}

/// Parse device metadata: null-terminated strings for name, model number,
/// manufacturer. Later firmware/serial/encrypted fields are opaque and discarded.
///
/// Layout: `04 00 04 00 1D [6 skip bytes] name\0 model\0 manufacturer\0`
fn parse_metadata(packet: &[u8]) -> Result<ParsedPacket, ParseError> {
    if !packet.starts_with(&packets::METADATA_PREFIX) {
        return Err(ParseError::Metadata("bad header".into()));
    }

    let skip = packets::METADATA_PREFIX.len() + 6;
    if packet.len() < skip + 3 {
        return Err(ParseError::Metadata("too short".into()));
    }

    let data = &packet[skip..];
    let mut strings = Vec::new();
    let mut start = 0;
    for i in 0..data.len() {
        if data[i] == 0x00 {
            let s = std::str::from_utf8(&data[start..i])
                .map_err(|_| ParseError::Metadata("invalid UTF-8".into()))?
                .to_owned();
            strings.push(s);
            start = i + 1;
            if strings.len() == 3 {
                break;
            }
        }
    }

    if strings.len() != 3 {
        return Err(ParseError::Metadata(
            "three terminated strings required".into(),
        ));
    }
    // Real metadata continues with serials, firmware and binary extensions. The
    // outer frame bound still applies; do not retain these private opaque fields.

    Ok(ParsedPacket::Metadata {
        name: strings[0].clone(),
        model_number: strings[1].clone(),
        manufacturer: strings[2].clone(),
    })
}

/// Parse proximity keys response.
///
/// Layout: `04 00 04 00 31 00 [count] ([type] [len_hi] [len_lo] [reserved] [key_bytes...])*`
fn parse_proximity_keys(packet: &[u8]) -> Result<ParsedPacket, ParseError> {
    if packet.len() < 7 {
        return Err(ParseError::TooShort(packet.len()));
    }

    let count = packet[6] as usize;
    if count > 16 {
        return Err(ParseError::Malformed("too many proximity keys"));
    }
    let mut keys = Vec::with_capacity(count);
    let mut offset = 7;

    for _ in 0..count {
        if offset + 4 > packet.len() {
            return Err(ParseError::Malformed("truncated key header"));
        }
        let key_type_byte = packet[offset];
        let key_len = u16::from_be_bytes([packet[offset + 1], packet[offset + 2]]) as usize;
        if packet[offset + 3] != 0 {
            return Err(ParseError::Malformed("nonzero key reserved byte"));
        }
        offset += 4; // type + len_hi + len_lo + reserved

        if offset + key_len > packet.len() {
            return Err(ParseError::Malformed("truncated key data"));
        }

        let data = packet[offset..offset + key_len].to_vec();
        offset += key_len;

        if let Some(kt) = ProximityKeyType::from_byte(key_type_byte) {
            keys.push(ProximityKey { key_type: kt, data });
        } else {
            return Ok(ParsedPacket::Unknown(packet.to_vec()));
        }
    }
    if offset != packet.len() {
        return Err(ParseError::Malformed("unexpected trailing key bytes"));
    }

    Ok(ParsedPacket::ProximityKeys(keys))
}

/// Parse magic cloud keys response.
///
/// Header: `04 00 04 00 31 00 02`, then two 16-byte TLV blocks.
fn parse_magic_cloud_keys(packet: &[u8]) -> Result<ParsedPacket, ParseError> {
    if packet.len() != 47 {
        return Err(ParseError::Malformed("magic key response must be 47 bytes"));
    }

    let mut idx = packets::MAGIC_CLOUD_KEYS_HEADER.len();

    // First TLV block — IRK (type 0x01)
    if packet[idx] != 0x01 {
        return Err(ParseError::MagicCloudKeys("expected IRK type 0x01".into()));
    }
    idx += 1;
    let len1 = ((packet[idx] as u16) << 8) | packet[idx + 1] as u16;
    if len1 != 16 {
        return Err(ParseError::MagicCloudKeys(format!(
            "IRK length {len1}, expected 16"
        )));
    }
    if packet[idx + 2] != 0 {
        return Err(ParseError::Malformed("nonzero key reserved byte"));
    }
    idx += 3; // len (2 bytes) + reserved (1 byte)
    let mut irk = [0u8; 16];
    irk.copy_from_slice(&packet[idx..idx + 16]);
    idx += 16;

    // Second TLV block — EncKey (type 0x04)
    if idx >= packet.len() || packet[idx] != 0x04 {
        return Err(ParseError::MagicCloudKeys(
            "expected EncKey type 0x04".into(),
        ));
    }
    idx += 1;
    let len2 = ((packet[idx] as u16) << 8) | packet[idx + 1] as u16;
    if len2 != 16 {
        return Err(ParseError::MagicCloudKeys(format!(
            "EncKey length {len2}, expected 16"
        )));
    }
    if packet[idx + 2] != 0 {
        return Err(ParseError::Malformed("nonzero key reserved byte"));
    }
    idx += 3;
    let mut enc_key = [0u8; 16];
    if idx + 16 > packet.len() {
        return Err(ParseError::TooShort(packet.len()));
    }
    enc_key.copy_from_slice(&packet[idx..idx + 16]);

    Ok(ParsedPacket::MagicCloudKeys { irk, enc_key })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_handshake_ack() {
        let pkt = [0x01, 0x00, 0x04, 0x00, 0x01, 0x02];
        match parse(&pkt).unwrap() {
            ParsedPacket::HandshakeAck => {}
            other => panic!("expected HandshakeAck, got {:?}", other),
        }
    }

    #[test]
    fn parse_ear_detection() {
        // Primary in ear (0x00), secondary out (0x01)
        let pkt = [0x04, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00, 0x01];
        match parse(&pkt).unwrap() {
            ParsedPacket::EarStatus { primary, secondary } => {
                assert_eq!(primary, WearState::InEar);
                assert_eq!(secondary, WearState::OutOfEar);
            }
            other => panic!("expected EarDetection, got {:?}", other),
        }
    }

    #[test]
    fn parse_noise_control() {
        // Noise control: adaptive (0x04)
        let pkt = [
            0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x0D, 0x04, 0x00, 0x00, 0x00,
        ];
        match parse(&pkt).unwrap() {
            ParsedPacket::NoiseControl(mode) => {
                assert_eq!(mode, NoiseControlMode::Adaptive);
            }
            other => panic!("expected NoiseControl, got {:?}", other),
        }
    }

    #[test]
    fn parse_features_ack() {
        let pkt = [0x04, 0x00, 0x04, 0x00, 0x2B, 0x00, 0x01];
        match parse(&pkt).unwrap() {
            ParsedPacket::FeaturesAck => {}
            other => panic!("expected FeaturesAck, got {:?}", other),
        }
    }

    #[test]
    fn parse_battery_packet() {
        // Example from AAP Definitions.md
        let pkt: Vec<u8> = vec![
            0x04, 0x00, 0x04, 0x00, 0x04, 0x00, 0x03, // count = 3
            0x02, 0x01, 0x64, 0x02, 0x01, // Right: 100%, discharging
            0x04, 0x01, 0x63, 0x01, 0x01, // Left: 99%, charging
            0x08, 0x01, 0x11, 0x02, 0x01, // Case: 17%, discharging
        ];
        match parse(&pkt).unwrap() {
            ParsedPacket::Battery(info) => {
                assert_eq!(info.left.level, 99);
                assert_eq!(info.right.level, 100);
                assert_eq!(info.case_.level, 17);
            }
            other => panic!("expected Battery, got {:?}", other),
        }
    }

    #[test]
    fn parse_conversational_awareness() {
        // 04 00 04 00 4B 00 02 00 01 [level]
        let pkt = [0x04, 0x00, 0x04, 0x00, 0x4B, 0x00, 0x02, 0x00, 0x01, 0x05];
        match parse(&pkt).unwrap() {
            ParsedPacket::ConversationalAwareness { status } => assert_eq!(status, 0x05),
            other => panic!("expected ConversationalAwareness, got {:?}", other),
        }
    }

    #[test]
    fn parse_control_command_notification() {
        // ConversationDetectConfig (0x28), data = 01 00 00 00
        let pkt = [
            0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x28, 0x01, 0x00, 0x00, 0x00,
        ];
        match parse(&pkt).unwrap() {
            ParsedPacket::ControlCommand { id, data } => {
                assert_eq!(id, ControlCommandId::ConversationDetectConfig);
                assert_eq!(data, [0x01, 0x00, 0x00, 0x00]);
            }
            other => panic!("expected ControlCommand, got {:?}", other),
        }
    }

    #[test]
    fn parse_unknown_control_command_is_unknown() {
        let pkt = [
            0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00,
        ];
        match parse(&pkt).unwrap() {
            ParsedPacket::Unknown(data) => assert_eq!(data, pkt),
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn parse_metadata() {
        let mut pkt = vec![0x04, 0x00, 0x04, 0x00, 0x1D, 0, 0, 0, 0, 0, 0];
        pkt.extend_from_slice(b"AirPods\0A3047\0Apple\0");
        match parse(&pkt).unwrap() {
            ParsedPacket::Metadata {
                name,
                model_number,
                manufacturer,
            } => {
                assert_eq!(name, "AirPods");
                assert_eq!(model_number, "A3047");
                assert_eq!(manufacturer, "Apple");
            }
            other => panic!("expected Metadata, got {:?}", other),
        }
    }

    #[test]
    fn parse_head_tracking() {
        let mut pkt = vec![0x04, 0x00, 0x04, 0x00, 0x17, 0x00];
        pkt.resize(70, 0x00);
        match parse(&pkt).unwrap() {
            ParsedPacket::HeadTracking(data) => assert_eq!(data.len(), 70),
            other => panic!("expected HeadTracking, got {:?}", other),
        }
    }

    #[test]
    fn parse_proximity_keys_single() {
        // 04 00 04 00 31 00 [count=1] [01 00 10 00] [16-byte IRK]
        let mut pkt = vec![0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x01];
        pkt.extend_from_slice(&[0x01, 0x00, 0x10, 0x00]);
        pkt.extend_from_slice(&[0xAA; 16]);
        match parse(&pkt).unwrap() {
            ParsedPacket::ProximityKeys(keys) => {
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0].key_type, ProximityKeyType::Irk);
                assert_eq!(keys[0].data, vec![0xAA; 16]);
            }
            other => panic!("expected ProximityKeys, got {:?}", other),
        }
    }

    #[test]
    fn parse_magic_cloud_keys() {
        // 04 00 04 00 31 00 02 | IRK TLV | EncKey TLV
        let mut pkt = vec![0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x02];
        pkt.extend_from_slice(&[0x01, 0x00, 0x10, 0x00]);
        pkt.extend_from_slice(&[0x11; 16]);
        pkt.extend_from_slice(&[0x04, 0x00, 0x10, 0x00]);
        pkt.extend_from_slice(&[0x22; 16]);
        match parse(&pkt).unwrap() {
            ParsedPacket::MagicCloudKeys { irk, enc_key } => {
                assert_eq!(irk, [0x11; 16]);
                assert_eq!(enc_key, [0x22; 16]);
            }
            other => panic!("expected MagicCloudKeys, got {:?}", other),
        }
    }

    #[test]
    fn two_key_response_falls_back_to_proximity_keys() {
        // count = 2 but both keys are IRK, so the strict IRK+EncKey magic parse
        // fails and the generic proximity parser must take over.
        let mut pkt = vec![0x04, 0x00, 0x04, 0x00, 0x31, 0x00, 0x02];
        pkt.extend_from_slice(&[0x01, 0x00, 0x10, 0x00]);
        pkt.extend_from_slice(&[0x33; 16]);
        pkt.extend_from_slice(&[0x01, 0x00, 0x10, 0x00]);
        pkt.extend_from_slice(&[0x44; 16]);
        match parse(&pkt).unwrap() {
            ParsedPacket::ProximityKeys(keys) => assert_eq!(keys.len(), 2),
            other => panic!("expected ProximityKeys, got {:?}", other),
        }
    }

    #[test]
    fn parse_unknown_opcode() {
        let pkt = [0x04, 0x00, 0x04, 0x00, 0xEE, 0x00];
        match parse(&pkt).unwrap() {
            ParsedPacket::Unknown(data) => assert_eq!(data, pkt),
            other => panic!("expected Unknown, got {:?}", other),
        }
    }
}
