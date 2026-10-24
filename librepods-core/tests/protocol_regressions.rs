// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Captured-frame shape and adversarial-input regressions, without hardware.

use librepods_core::device::{battery, ble_advert, state::NoiseControlMode};
use librepods_core::protocol::{
    control_command::{ControlCommand, ControlCommandId},
    packets,
    parser::{self, ParsedPacket},
};
use librepods_core::transport::TransportError;

#[test]
fn parsers_reject_truncated_and_malformed_known_packets() {
    for bytes in [
        vec![],
        vec![4, 0, 4],
        vec![0, 0, 0, 0],
        vec![4, 0, 4, 0],
        vec![4, 0, 4, 0, 9, 1],
        vec![4, 0, 4, 0, 6, 0, 0],
        vec![4, 0, 4, 0, 6, 0, 0, 0, 0],
        vec![4, 0, 4, 0, 9, 0, 1],
        vec![4, 0, 4, 0, 9, 0, 1, 1, 0, 0, 0, 0],
        vec![4, 0, 4, 0, 0x17, 0],
        vec![4; 1025],
    ] {
        assert!(
            parser::parse(&bytes).is_err(),
            "accepted malformed {bytes:02x?}"
        );
    }
    for bytes in [
        vec![4, 0, 4, 0, 0x1d, 0],
        vec![4, 0, 4, 0, 0x31, 0],
        vec![4, 0, 4, 0, 0x31, 0, 17],
        vec![4, 0, 4, 0, 0x31, 0, 1],
        vec![4, 0, 4, 0, 0x31, 0, 1, 1, 0, 16, 0],
        vec![4, 0, 4, 0, 0x31, 0, 1, 1, 0, 0, 1],
        vec![4, 0, 4, 0, 0x31, 0, 0, 1],
    ] {
        assert!(parser::parse(&bytes).is_err());
    }
    let mut metadata = vec![4, 0, 4, 0, 0x1d, 0, 0, 0, 0, 0, 0];
    metadata.extend_from_slice(b"Name\0Model\0unterminated");
    assert!(parser::parse(&metadata).is_err());
    let mut metadata = vec![4, 0, 4, 0, 0x1d, 0, 0, 0, 0, 0, 0];
    metadata.extend_from_slice(b"Name\0Model\0\xff\0");
    assert!(parser::parse(&metadata).is_err());
    let mut metadata = vec![4, 0, 4, 0, 0x1d, 0, 0, 0, 0, 0, 0];
    metadata.extend_from_slice(b"Name\0Model\0Apple\0extension");
    assert!(matches!(
        parser::parse(&metadata),
        Ok(ParsedPacket::Metadata { .. })
    ));
}

#[test]
fn unknown_values_are_preserved_instead_of_becoming_valid_defaults() {
    let unknown_mode = ControlCommand::create(0x0d, &[99, 0, 0, 0]);
    assert!(
        matches!(parser::parse(&unknown_mode),Ok(ParsedPacket::Unknown(raw)) if raw==unknown_mode)
    );
    let extended_mode = ControlCommand::create(0x0d, &[1, 1, 0, 0]);
    assert!(matches!(
        parser::parse(&extended_mode),
        Ok(ParsedPacket::Unknown(_))
    ));
    let unknown_ca = vec![4, 0, 4, 0, 0x4b, 0, 1, 2];
    assert!(matches!(parser::parse(&unknown_ca),Ok(ParsedPacket::Unknown(raw)) if raw==unknown_ca));
    let unknown_key = vec![4, 0, 4, 0, 0x31, 0, 1, 99, 0, 0, 0];
    assert!(
        matches!(parser::parse(&unknown_key),Ok(ParsedPacket::Unknown(raw)) if raw==unknown_key)
    );
    let header = [4, 0, 4, 0, 4, 0];
    assert!(battery::parse_battery(&header).is_err());
    assert!(battery::parse_battery(&[0; 7]).is_err());
    assert!(battery::parse_battery(&[4, 0, 4, 0, 4, 0, 33]).is_err());
    assert!(battery::parse_battery(&[4, 0, 4, 0, 4, 0, 1, 2, 0, 10, 2, 1]).is_err());
    assert!(battery::parse_battery(&[4, 0, 4, 0, 4, 0, 1, 2, 1, 10, 2, 0]).is_err());
}

#[test]
fn key_response_lengths_are_full_width_and_never_partially_accepted() {
    let mut oversized = vec![4, 0, 4, 0, 0x31, 0, 1, 1, 1, 0, 0];
    oversized.extend_from_slice(&[0; 16]);
    assert!(parser::parse(&oversized).is_err());
    let mut truncated = vec![4, 0, 4, 0, 0x31, 0, 2, 1, 0, 16, 0];
    truncated.extend_from_slice(&[1; 16]);
    assert!(parser::parse(&truncated).is_err());
    let mut bad_reserved = vec![4, 0, 4, 0, 0x31, 0, 2, 1, 0, 16, 1];
    bad_reserved.extend_from_slice(&[1; 16]);
    bad_reserved.extend_from_slice(&[4, 0, 16, 0]);
    bad_reserved.extend_from_slice(&[2; 16]);
    assert!(parser::parse(&bad_reserved).is_err());
    bad_reserved[10] = 0;
    bad_reserved[30] = 1;
    assert!(parser::parse(&bad_reserved).is_err());
    bad_reserved[30] = 0;
    bad_reserved[29] = 15;
    assert!(parser::parse(&bad_reserved).is_err());
    bad_reserved[29] = 16;
    bad_reserved[9] = 15;
    assert!(parser::parse(&bad_reserved).is_err());
}

#[test]
fn packet_builders_and_control_helpers_cover_all_known_identifiers() {
    for byte in 0..=255 {
        if let Some(id) = ControlCommandId::from_byte(byte) {
            assert_eq!(id.as_byte(), byte);
            let raw = ControlCommand::enabled(byte);
            assert_eq!(ControlCommand::parse_id(&raw), Some(byte));
            assert_eq!(ControlCommand::parse_value(&raw), Some(1));
            assert_eq!(ControlCommand::parse_state(&raw), Some(true));
            assert_eq!(
                ControlCommand::parse_state(&ControlCommand::disabled(byte)),
                Some(false)
            );
        }
    }
    for size in 0..11 {
        let packet = vec![0; size];
        assert_eq!(ControlCommand::parse_data(&packet), None);
        assert_eq!(ControlCommand::parse_id(&packet), None);
        assert_eq!(ControlCommand::parse_value(&packet), None);
        assert_eq!(ControlCommand::parse_state(&packet), None);
    }
    assert_eq!(
        ControlCommand::parse_state(&ControlCommand::create(1, &[9, 0, 0, 0])),
        None
    );
    assert_eq!(ControlCommand::parse_data(&[0; 11]), None);
    for value in 1..=4 {
        let mode = NoiseControlMode::from_byte(value).expect("known mode");
        assert_eq!(
            packets::build_noise_control(mode),
            ControlCommand::create(0x0d, &[value, 0, 0, 0])
        );
    }
    assert_eq!(NoiseControlMode::from_byte(0), None);
    assert_eq!(
        packets::build_adaptive_noise_level(255),
        ControlCommand::create(0x2e, &[100, 0, 0, 0])
    );
    assert_eq!(
        packets::build_request_proximity_keys(5),
        [4, 0, 4, 0, 0x30, 0, 5, 0]
    );
    let renamed = packets::build_rename("Pods");
    assert_eq!(&renamed[9..], b"Pods");
    assert_eq!(renamed[7], 4);
    assert!(!packets::is_head_tracking_data(&[0; 60]));
    assert!(!packets::is_head_tracking_data(&[0; 70]));
    let mut tracking = vec![4, 0, 4, 0, 0x17, 0, 0, 0, 0x10, 0, 0, 0];
    tracking.resize(70, 0);
    assert!(!packets::is_head_tracking_data(&tracking));
    tracking[10] = 0x44;
    assert!(packets::is_head_tracking_data(&tracking));
    tracking[11] = 1;
    assert!(!packets::is_head_tracking_data(&tracking));
}

#[test]
fn transport_error_codes_and_retryability_do_not_depend_on_display_text() {
    let cases = [
        (TransportError::Timeout, "timeout", true),
        (TransportError::Cancelled, "cancelled", false),
        (TransportError::Unsupported, "unsupported", false),
        (
            TransportError::Truncated { limit: 1024 },
            "truncated",
            false,
        ),
        (TransportError::PermissionDenied, "permission_denied", false),
        (TransportError::NotConnected, "not_connected", true),
        (TransportError::ConnectionClosed, "connection_closed", true),
        (TransportError::ChannelClosed, "channel_closed", true),
        (
            TransportError::with_source("context", std::io::Error::other("cause")),
            "io",
            true,
        ),
        (TransportError::new("other"), "internal", false),
    ];
    for (error, code, retryable) in cases {
        assert_eq!(error.code(), code);
        assert_eq!(error.retryable(), retryable);
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn arbitrary_and_structured_frames_never_panic_or_allocate_from_untrusted_counts() {
    let mut seed = 0x4f504f44_u64;
    for size in 0..=1100 {
        let mut bytes = vec![0; size];
        for byte in &mut bytes {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (seed >> 32) as u8;
        }
        let _ = parser::parse(&bytes);
        let _ = battery::parse_battery(&bytes);
        let _ = ble_advert::parse_advertisement(&bytes);
        if size >= 6 {
            bytes[..4].copy_from_slice(&packets::HEADER);
            bytes[5] = 0;
            let _ = parser::parse(&bytes);
        }
    }
}

#[test]
fn full_metadata_extensions_do_not_hide_verified_identity_or_retain_serials() {
    let fixture = include_str!("fixtures/metadata_extended.hex");
    let hex = fixture
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<String>();
    let packet = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let identity_end = packet[11..]
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == 0)
        .nth(2)
        .unwrap()
        .0
        + 12;
    for end in 0..identity_end {
        assert!(parser::parse(&packet[..end]).is_err());
    }
    for end in identity_end..=packet.len() {
        assert!(matches!(
            parser::parse(&packet[..end]),
            Ok(ParsedPacket::Metadata { .. })
        ));
    }
    let parsed = parser::parse(&packet).unwrap();
    assert!(
        matches!(&parsed,ParsedPacket::Metadata{name,model_number,manufacturer} if name=="Example AirPods Pro"&&model_number=="A3048"&&manufacturer=="Apple Inc.")
    );
    assert!(!format!("{parsed:?}").contains("REDACTED"));
    let mut state = librepods_core::device::state::DeviceState::default();
    state.begin_session(1);
    assert!(state.apply_packet(&parsed, 1, 0));
    assert!(state.fresh("metadata"));
    assert!(state.model.capabilities().noise_control);
}
