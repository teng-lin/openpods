// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Device state model, AirPods model enum, noise control mode.
//!
//! References:
//! - `deviceinfo.hpp` — `DeviceInfo` QObject
//! - `enums.h` — `NoiseControlMode`, `AirPodsModel`, `parseModelNumber()`

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::device::battery::BatteryInfo;
use crate::protocol::control_command::{ControlCommand, ControlCommandId};
use crate::protocol::parser::{ParsedPacket, ProximityKeyType};

// ---------------------------------------------------------------------------
// Noise control
// ---------------------------------------------------------------------------

/// Active noise control mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum NoiseControlMode {
    Off = 0x01,
    NoiseCancellation = 0x02,
    Transparency = 0x03,
    Adaptive = 0x04,
}

impl NoiseControlMode {
    /// Wire value used in AAP control commands (ListeningMode identifier 0x0D).
    pub fn as_byte(self) -> u8 {
        self as u8
    }

    /// Parse from wire value.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Off),
            0x02 => Some(Self::NoiseCancellation),
            0x03 => Some(Self::Transparency),
            0x04 => Some(Self::Adaptive),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// AirPods model
// ---------------------------------------------------------------------------

/// Known AirPods hardware models.
///
/// Model numbers sourced from <https://support.apple.com/en-us/109525>
/// and `enums.h` `parseModelNumber()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AirPodsModel {
    #[default]
    Unknown,
    AirPods1,
    AirPods2,
    AirPods3,
    AirPodsPro,
    AirPodsPro2Lightning,
    AirPodsPro2Usbc,
    AirPodsMaxLightning,
    AirPodsMaxUsbc,
    AirPods4,
    AirPods4Anc,
    AirPodsPro3,
    AirPodsMax2,
}

/// Map an Apple model number string (e.g. `"A3047"`) to an [`AirPodsModel`].
pub fn model_from_number(model_number: &str) -> AirPodsModel {
    match model_number {
        "A1523" | "A1722" => AirPodsModel::AirPods1,
        "A2032" | "A2031" => AirPodsModel::AirPods2,
        "A2565" | "A2564" => AirPodsModel::AirPods3,
        "A2084" | "A2083" => AirPodsModel::AirPodsPro,
        "A2931" | "A2699" | "A2698" => AirPodsModel::AirPodsPro2Lightning,
        "A3047" | "A3048" | "A3049" => AirPodsModel::AirPodsPro2Usbc,
        "A2096" => AirPodsModel::AirPodsMaxLightning,
        "A3184" => AirPodsModel::AirPodsMaxUsbc,
        "A3053" | "A3050" | "A3054" => AirPodsModel::AirPods4,
        "A3056" | "A3055" | "A3057" => AirPodsModel::AirPods4Anc,
        "A3063" | "A3064" | "A3065" => AirPodsModel::AirPodsPro3,
        "A3454" => AirPodsModel::AirPodsMax2,
        _ => AirPodsModel::Unknown,
    }
}

/// Verified feature map from the Omarchy daemon's enums.h; unknown models fail closed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub noise_control: bool,
    pub noise_off: bool,
    pub adaptive: bool,
    pub conversational_awareness: bool,
    pub one_bud_anc: bool,
    pub headset: bool,
}

impl AirPodsModel {
    pub fn capabilities(self) -> ModelCapabilities {
        let noise_control = matches!(
            self,
            Self::AirPodsPro
                | Self::AirPodsPro2Lightning
                | Self::AirPodsPro2Usbc
                | Self::AirPodsPro3
                | Self::AirPods4Anc
                | Self::AirPodsMaxLightning
                | Self::AirPodsMaxUsbc
                | Self::AirPodsMax2
        );
        let adaptive = matches!(
            self,
            Self::AirPodsPro2Lightning
                | Self::AirPodsPro2Usbc
                | Self::AirPodsPro3
                | Self::AirPods4Anc
                | Self::AirPodsMax2
        );
        ModelCapabilities {
            noise_control,
            noise_off: noise_control && self != Self::AirPodsPro3,
            adaptive,
            conversational_awareness: adaptive,
            one_bud_anc: matches!(
                self,
                Self::AirPodsPro
                    | Self::AirPodsPro2Lightning
                    | Self::AirPodsPro2Usbc
                    | Self::AirPodsPro3
                    | Self::AirPods4Anc
            ),
            headset: matches!(
                self,
                Self::AirPodsMaxLightning | Self::AirPodsMaxUsbc | Self::AirPodsMax2
            ),
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Unknown => "",
            Self::AirPods1 => "AirPods",
            Self::AirPods2 => "AirPods (2nd generation)",
            Self::AirPods3 => "AirPods (3rd generation)",
            Self::AirPodsPro => "AirPods Pro",
            Self::AirPodsPro2Lightning | Self::AirPodsPro2Usbc => "AirPods Pro 2",
            Self::AirPodsPro3 => "AirPods Pro 3",
            Self::AirPodsMaxLightning | Self::AirPodsMaxUsbc => "AirPods Max",
            Self::AirPodsMax2 => "AirPods Max 2",
            Self::AirPods4 | Self::AirPods4Anc => "AirPods 4",
        }
    }

    pub fn legacy_index(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WearState {
    InEar,
    OutOfEar,
    InCase,
    Unknown(u8),
}

impl WearState {
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::InEar,
            1 => Self::OutOfEar,
            2 => Self::InCase,
            byte => Self::Unknown(byte),
        }
    }
    pub fn in_ear(self) -> Option<bool> {
        match self {
            Self::InEar => Some(true),
            Self::OutOfEar | Self::InCase => Some(false),
            Self::Unknown(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WearReport {
    pub primary: WearState,
    pub secondary: WearState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationSource {
    Aacp,
    Ble,
    Stored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationMeta {
    pub generation: u64,
    pub observed_at_ms: u64,
    pub source: ObservationSource,
    pub stale: bool,
}

/// Map a BLE advertisement model ID (big-endian u16) to an [`AirPodsModel`].
///
/// Source: `BLEManager.kt` `modelNames` map.
pub fn model_from_ble_id(id: u16) -> AirPodsModel {
    match id {
        0x0220 => AirPodsModel::AirPods1,
        0x0F20 => AirPodsModel::AirPods2,
        0x1320 => AirPodsModel::AirPods3,
        0x0E20 => AirPodsModel::AirPodsPro,
        0x1420 => AirPodsModel::AirPodsPro2Lightning,
        0x2420 => AirPodsModel::AirPodsPro2Usbc,
        0x0A20 => AirPodsModel::AirPodsMaxLightning,
        0x1F20 => AirPodsModel::AirPodsMaxUsbc,
        0x1920 => AirPodsModel::AirPods4,
        0x1B20 => AirPodsModel::AirPods4Anc,
        _ => AirPodsModel::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Device state
// ---------------------------------------------------------------------------

/// Aggregate device state — the central "truth" struct updated by the parser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    #[serde(default)]
    pub session_generation: u64,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub observations: BTreeMap<String, ObservationMeta>,
    #[serde(default)]
    pub wear: Option<WearReport>,
    pub device_name: String,
    pub model_number: String,
    pub manufacturer: String,
    pub model: AirPodsModel,
    pub bluetooth_address: String,

    pub battery: BatteryInfo,
    pub noise_control: NoiseControlMode,
    pub conversational_awareness: bool,
    pub primary_in_ear: bool,
    pub secondary_in_ear: bool,
    pub adaptive_noise_level: u8,
    pub one_bud_anc: bool,

    /// Settings actually reported during this connection, including unknown identifiers.
    #[serde(default)]
    pub reported_controls: BTreeMap<u8, [u8; 4]>,
    #[serde(default)]
    pub noise_control_reported: bool,
    #[serde(default)]
    pub ear_detection_reported: bool,

    /// Identity Resolving Key for BLE RPA verification (16 bytes).
    #[serde(skip)]
    pub magic_acc_irk: Option<[u8; 16]>,
    /// Encryption key for BLE ad battery decryption (16 bytes).
    #[serde(skip)]
    pub magic_acc_enc_key: Option<[u8; 16]>,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            session_generation: 0,
            revision: 0,
            connected: false,
            observations: BTreeMap::new(),
            wear: None,
            device_name: String::new(),
            model_number: String::new(),
            manufacturer: String::new(),
            model: AirPodsModel::Unknown,
            bluetooth_address: String::new(),
            battery: BatteryInfo::default(),
            noise_control: NoiseControlMode::Off,
            conversational_awareness: false,
            primary_in_ear: false,
            secondary_in_ear: false,
            adaptive_noise_level: 50,
            one_bud_anc: false,
            reported_controls: BTreeMap::new(),
            noise_control_reported: false,
            ear_detection_reported: false,
            magic_acc_irk: None,
            magic_acc_enc_key: None,
        }
    }
}

impl DeviceState {
    pub fn capabilities(&self) -> ModelCapabilities {
        self.model.capabilities()
    }

    pub fn begin_session(&mut self, generation: u64) {
        if generation <= self.session_generation {
            return;
        }
        for observation in self.observations.values_mut() {
            observation.stale = true;
        }
        self.session_generation = generation;
        self.connected = true;
        self.reported_controls.clear();
        self.noise_control_reported = false;
        self.ear_detection_reported = false;
        self.wear = None;
        self.primary_in_ear = false;
        self.secondary_in_ear = false;
        self.revision = self.revision.saturating_add(1);
    }

    pub fn end_session(&mut self, generation: u64) {
        if generation != self.session_generation {
            return;
        }
        self.connected = false;
        self.reported_controls.clear();
        self.noise_control_reported = false;
        self.ear_detection_reported = false;
        self.wear = None;
        for observation in self.observations.values_mut() {
            observation.stale = true;
        }
        self.revision = self.revision.saturating_add(1);
    }

    pub fn fresh(&self, field: &str) -> bool {
        self.observations
            .get(field)
            .is_some_and(|meta| !meta.stale && meta.generation == self.session_generation)
    }

    /// The only reducer for device notifications; platform effects stay with the caller.
    pub fn apply_packet(
        &mut self,
        packet: &ParsedPacket,
        generation: u64,
        observed_at_ms: u64,
    ) -> bool {
        if !self.connected || generation != self.session_generation {
            return false;
        }
        let mut fields = Vec::new();
        match packet {
            ParsedPacket::Battery(info) => {
                self.battery.merge(info);
                for component in &info.present {
                    fields.push(format!("battery.{}", component.name().to_lowercase()));
                }
            }
            ParsedPacket::EarStatus { primary, secondary } => {
                self.wear = Some(WearReport {
                    primary: *primary,
                    secondary: *secondary,
                });
                self.primary_in_ear = primary.in_ear() == Some(true);
                self.secondary_in_ear = secondary.in_ear() == Some(true);
                self.ear_detection_reported = true;
                fields.push("wear".into());
            }
            ParsedPacket::EarDetection {
                primary_in_ear,
                secondary_in_ear,
            } => {
                self.wear = Some(WearReport {
                    primary: if *primary_in_ear {
                        WearState::InEar
                    } else {
                        WearState::OutOfEar
                    },
                    secondary: if *secondary_in_ear {
                        WearState::InEar
                    } else {
                        WearState::OutOfEar
                    },
                });
                self.primary_in_ear = *primary_in_ear;
                self.secondary_in_ear = *secondary_in_ear;
                self.ear_detection_reported = true;
                fields.push("wear".into());
            }
            ParsedPacket::NoiseControl(mode) => {
                self.noise_control = *mode;
                self.noise_control_reported = true;
                self.reported_controls.insert(
                    ControlCommandId::ListeningMode.as_byte(),
                    [mode.as_byte(), 0, 0, 0],
                );
                fields.push("noise_control".into());
                fields.push(format!(
                    "control.{:02x}",
                    ControlCommandId::ListeningMode.as_byte()
                ));
            }
            ParsedPacket::ControlCommand { id, data } => {
                self.reported_controls.insert(id.as_byte(), *data);
                match id {
                    ControlCommandId::ConversationDetectConfig if matches!(data[0], 1 | 2) => {
                        self.conversational_awareness = data[0] == 1
                    }
                    ControlCommandId::OneBudAncMode if matches!(data[0], 1 | 2) => {
                        self.one_bud_anc = data[0] == 1
                    }
                    ControlCommandId::AutoAncStrength if data[0] <= 100 => {
                        self.adaptive_noise_level = data[0]
                    }
                    _ => {}
                }
                fields.push(format!("control.{:02x}", id.as_byte()));
            }
            ParsedPacket::Metadata {
                name,
                model_number,
                manufacturer,
            } => {
                self.device_name.clone_from(name);
                self.model_number.clone_from(model_number);
                self.manufacturer.clone_from(manufacturer);
                self.model = model_from_number(model_number);
                fields.push("metadata".into());
            }
            ParsedPacket::ProximityKeys(keys) => {
                for key in keys {
                    if let Ok(value) = <[u8; 16]>::try_from(key.data.as_slice()) {
                        match key.key_type {
                            ProximityKeyType::Irk => self.magic_acc_irk = Some(value),
                            ProximityKeyType::EncKey => self.magic_acc_enc_key = Some(value),
                        }
                    }
                }
                fields.push("keys".into());
            }
            ParsedPacket::MagicCloudKeys { irk, enc_key } => {
                self.magic_acc_irk = Some(*irk);
                self.magic_acc_enc_key = Some(*enc_key);
                fields.push("keys".into());
            }
            ParsedPacket::Unknown(raw) => {
                if let (Some(id), Some(data)) = (
                    ControlCommand::parse_id(raw),
                    ControlCommand::parse_data(raw),
                ) {
                    self.reported_controls.insert(id, data);
                    fields.push(format!("control.{id:02x}"));
                }
            }
            ParsedPacket::ConversationalAwareness { .. }
            | ParsedPacket::HeadTracking(_)
            | ParsedPacket::HandshakeAck
            | ParsedPacket::FeaturesAck => {}
        }
        if fields.is_empty() {
            return false;
        }
        for field in fields {
            self.observations.insert(
                field,
                ObservationMeta {
                    generation,
                    observed_at_ms,
                    source: ObservationSource::Aacp,
                    stale: false,
                },
            );
        }
        self.revision = self.revision.saturating_add(1);
        true
    }
    /// At least one pod is in ear.
    pub fn any_pod_in_ear(&self) -> bool {
        self.primary_in_ear || self.secondary_in_ear
    }

    /// Both pods are in ear.
    pub fn both_pods_in_ear(&self) -> bool {
        self.primary_in_ear && self.secondary_in_ear
    }

    /// Reset all state (e.g. on disconnect).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lookup() {
        assert_eq!(model_from_number("A3047"), AirPodsModel::AirPodsPro2Usbc);
        assert_eq!(model_from_number("XXXX"), AirPodsModel::Unknown);
    }

    #[test]
    fn ble_model_lookup() {
        assert_eq!(model_from_ble_id(0x2420), AirPodsModel::AirPodsPro2Usbc);
        assert_eq!(model_from_ble_id(0xFFFF), AirPodsModel::Unknown);
    }

    #[test]
    fn noise_control_round_trip() {
        for mode in [
            NoiseControlMode::Off,
            NoiseControlMode::NoiseCancellation,
            NoiseControlMode::Transparency,
            NoiseControlMode::Adaptive,
        ] {
            assert_eq!(NoiseControlMode::from_byte(mode.as_byte()), Some(mode));
        }
    }
}
