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

use crate::device::battery::{BatteryInfo, Component};
use crate::device::ble_advert::{BleAdvertData, LidState};
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
#[derive(Clone, Serialize, Deserialize)]
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
    /// Absolute orientation proved by the wear report's source (currently verified BLE).
    /// AACP wear has no absolute side identifier. A preceding battery primary is
    /// historical context, not proof: the microphone/primary role can switch.
    #[serde(default)]
    pub wear_primary_side: Option<Component>,
    /// Last primary side observed through AACP in this session. BLE never changes it.
    /// This historical battery context never establishes a later wear report's orientation.
    #[serde(skip)]
    pub aacp_primary_side: Option<Component>,
    #[serde(default)]
    pub lid_state: LidState,
    #[serde(default)]
    pub ca_event: Option<u8>,
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

impl std::fmt::Debug for DeviceState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceState")
            .field("session_generation", &self.session_generation)
            .field("revision", &self.revision)
            .field("connected", &self.connected)
            .field("model", &self.model)
            .field("noise_control_reported", &self.noise_control_reported)
            .field(
                "keys_available",
                &(self.magic_acc_irk.is_some() && self.magic_acc_enc_key.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            session_generation: 0,
            revision: 0,
            connected: false,
            observations: BTreeMap::new(),
            wear: None,
            wear_primary_side: None,
            aacp_primary_side: None,
            lid_state: LidState::Unknown,
            ca_event: None,
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
    pub fn set_device_identity(&mut self, address: &str, name: &str) {
        if self.bluetooth_address != address {
            let generation = self.session_generation;
            let revision = self.revision;
            *self = Self::default();
            self.session_generation = generation;
            self.revision = revision.saturating_add(1);
        }
        self.bluetooth_address = address.to_owned();
        if !name.is_empty() {
            self.device_name = name.to_owned();
        }
    }

    pub fn capabilities(&self) -> ModelCapabilities {
        self.model.capabilities()
    }

    /// Explicit key removal also invalidates the reducer's private cached copy.
    pub fn forget_proximity_keys(&mut self) {
        let changed = self.magic_acc_irk.is_some()
            || self.magic_acc_enc_key.is_some()
            || self.observations.contains_key("keys");
        self.magic_acc_irk = None;
        self.magic_acc_enc_key = None;
        self.observations.remove("keys");
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
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
        self.wear_primary_side = None;
        self.aacp_primary_side = None;
        self.ca_event = None;
        self.primary_in_ear = false;
        self.secondary_in_ear = false;
        self.revision = self.revision.saturating_add(1);
    }

    pub fn end_session(&mut self, generation: u64) {
        if generation != self.session_generation || !self.connected {
            return;
        }
        self.connected = false;
        self.reported_controls.clear();
        self.noise_control_reported = false;
        self.ear_detection_reported = false;
        self.wear = None;
        self.wear_primary_side = None;
        self.aacp_primary_side = None;
        self.ca_event = None;
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

    fn accepts_ble(&self, field: &str, now_ms: u64, aacp_fresh_for_ms: u64) -> bool {
        let Some(previous) = self.observations.get(field) else {
            return true;
        };
        if previous.observed_at_ms > now_ms {
            return false;
        }
        previous.stale
            || previous.source != ObservationSource::Aacp
            || now_ms.saturating_sub(previous.observed_at_ms) > aacp_fresh_for_ms
    }

    /// Caller must resolve the advertisement identity before invoking this reducer.
    /// BLE telemetry never changes AACP readiness or command-confirmation state.
    pub fn apply_verified_ble(
        &mut self,
        advert: &BleAdvertData,
        generation: u64,
        observed_at_ms: u64,
        aacp_fresh_for_ms: u64,
    ) -> bool {
        use crate::device::battery::{BatteryState, BatteryStatus, Component};
        if generation != self.session_generation {
            return false;
        }
        let mut patch = BatteryInfo::default();
        let left = advert.encrypted_left_battery.or(advert.left_battery);
        let right = advert.encrypted_right_battery.or(advert.right_battery);
        let case = advert
            .encrypted_case_battery
            .or(advert.case_battery)
            .filter(|level| *level != 0 || advert.one_pod_in_case || advert.both_pods_in_case);
        let is_headset = self.capabilities().headset
            || (self.model == AirPodsModel::Unknown && advert.model.capabilities().headset);
        let readings = if is_headset {
            vec![(
                Component::Headset,
                left.or(right),
                advert
                    .encrypted_left_charging
                    .unwrap_or(advert.left_charging),
            )]
        } else {
            vec![
                (
                    Component::Left,
                    left,
                    advert
                        .encrypted_left_charging
                        .unwrap_or(advert.left_charging),
                ),
                (
                    Component::Right,
                    right,
                    advert
                        .encrypted_right_charging
                        .unwrap_or(advert.right_charging),
                ),
                (
                    Component::Case,
                    case,
                    advert
                        .encrypted_case_charging
                        .unwrap_or(advert.case_charging),
                ),
            ]
        };
        let mut fields = Vec::new();
        for (component, level, charging) in readings {
            let field = format!("battery.{}", component.name().to_lowercase());
            if let Some(level) = level.filter(|value| *value <= 100) {
                if !self.accepts_ble(&field, observed_at_ms, aacp_fresh_for_ms) {
                    continue;
                }
                let value = BatteryState {
                    level,
                    status: if charging {
                        BatteryStatus::Charging
                    } else {
                        BatteryStatus::Discharging
                    },
                };
                match component {
                    Component::Headset => patch.headset = value,
                    Component::Left => patch.left = value,
                    Component::Right => patch.right = value,
                    Component::Case => patch.case_ = value,
                }
                patch.present.push(component);
                fields.push(field);
            }
        }
        patch.primary_pod = if advert.primary_left {
            Component::Left
        } else {
            Component::Right
        };
        self.battery.merge(&patch);
        if self.accepts_ble("wear", observed_at_ms, aacp_fresh_for_ms) {
            self.wear_primary_side = (!is_headset).then_some(if advert.primary_left {
                Component::Left
            } else {
                Component::Right
            });
            let left = if advert.both_pods_in_case {
                WearState::InCase
            } else if advert.left_in_ear {
                WearState::InEar
            } else {
                WearState::OutOfEar
            };
            let right = if advert.both_pods_in_case {
                WearState::InCase
            } else if advert.right_in_ear {
                WearState::InEar
            } else {
                WearState::OutOfEar
            };
            self.wear = Some(if advert.primary_left {
                WearReport {
                    primary: left,
                    secondary: right,
                }
            } else {
                WearReport {
                    primary: right,
                    secondary: left,
                }
            });
            self.primary_in_ear = self
                .wear
                .is_some_and(|wear| wear.primary == WearState::InEar);
            self.secondary_in_ear = self
                .wear
                .is_some_and(|wear| wear.secondary == WearState::InEar);
            fields.push("wear".into());
        }
        if advert.lid_state != LidState::Unknown
            && self.accepts_ble("lid", observed_at_ms, aacp_fresh_for_ms)
        {
            self.lid_state = advert.lid_state;
            fields.push("lid".into());
        }
        if advert.model != AirPodsModel::Unknown && self.model == AirPodsModel::Unknown {
            self.model = advert.model;
            fields.push("model".into());
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
                    source: ObservationSource::Ble,
                    stale: false,
                },
            );
        }
        self.revision = self.revision.saturating_add(1);
        true
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
        if self.observations.values().any(|meta| {
            meta.generation == generation
                && meta.source == ObservationSource::Aacp
                && !meta.stale
                && meta.observed_at_ms > observed_at_ms
        }) {
            return false;
        }
        let mut fields = Vec::new();
        match packet {
            ParsedPacket::Battery(info) => {
                if let Some(side) = info.primary_side() {
                    self.aacp_primary_side = Some(side);
                }
                // A later battery notification does not retroactively orient an older wear report.
                self.battery.merge(info);
                for component in &info.present {
                    fields.push(format!("battery.{}", component.name().to_lowercase()));
                }
                for component in info.unknown_records.keys() {
                    fields.push(format!("battery.unknown.{component:02x}"));
                }
            }
            ParsedPacket::EarStatus { primary, secondary } => {
                self.wear_primary_side = None;
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
                self.wear_primary_side = None;
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
            ParsedPacket::ConversationalAwareness { status } => {
                self.ca_event = Some(*status);
                fields.push("ca_event".into());
            }
            ParsedPacket::HeadTracking(_)
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

#[cfg(test)]
mod reducer_tests {
    use super::*;
    use crate::device::battery::{self, BatteryStatus, Component};
    use crate::protocol::parser::{self, ProximityKey};
    fn reduce(state: &mut DeviceState, bytes: &[u8], at: u64) {
        assert!(state.apply_packet(&parser::parse(bytes).expect("fixture"), 1, at));
    }
    #[test]
    fn partial_updates_preserve_components_and_true_zero_is_available() {
        let mut state = DeviceState::default();
        state.begin_session(1);
        reduce(
            &mut state,
            &[4, 0, 4, 0, 4, 0, 2, 2, 1, 80, 2, 1, 8, 1, 50, 2, 1],
            1,
        );
        reduce(&mut state, &[4, 0, 4, 0, 4, 0, 1, 4, 1, 0, 1, 1], 2);
        assert_eq!(state.battery.left.level, 0);
        assert_eq!(state.battery.left.status, BatteryStatus::Charging);
        assert_eq!(state.battery.right.level, 80);
        assert_eq!(state.battery.case_.level, 50);
        assert_eq!(state.battery.primary_side(), Some(Component::Left));
        assert_eq!(state.battery.get(Component::Headset), Default::default());
        reduce(&mut state, &[4, 0, 4, 0, 4, 0, 1, 1, 1, 42, 2, 1], 3);
        assert_eq!(state.battery.get(Component::Headset).level, 42);
        reduce(&mut state, &[4, 0, 4, 0, 4, 0, 1, 0x7f, 1, 25, 9, 1], 4);
        assert_eq!(
            state.battery.unknown_records[&0x7f].status,
            BatteryStatus::Unknown(9)
        );
        assert!(state.fresh("battery.unknown.7f"));
        let revision = state.revision;
        assert!(!state.apply_packet(&ParsedPacket::NoiseControl(NoiseControlMode::Off), 0, 5));
        assert!(!state.apply_packet(&ParsedPacket::NoiseControl(NoiseControlMode::Off), 1, 3));
        assert_eq!(state.revision, revision);
        state.end_session(0);
        assert!(state.connected);
        state.end_session(1);
        assert!(!state.connected);
        assert!(!state.fresh("battery.right"));
        let revision = state.revision;
        state.end_session(1);
        assert_eq!(state.revision, revision);
        state.begin_session(2);
        assert!(!state.noise_control_reported);
        assert_eq!(state.battery.right.level, 80);
        assert!(!state.apply_packet(&ParsedPacket::NoiseControl(NoiseControlMode::Off), 1, 6));
        assert_eq!(
            battery::parse_battery(&[4, 0, 4, 0, 4, 0, 0])
                .expect("empty packet")
                .primary_side(),
            None
        );
    }
    #[test]
    fn only_reducer_populates_settings_keys_wear_and_metadata() {
        let mut state = DeviceState::default();
        assert!(!state.apply_packet(&ParsedPacket::HandshakeAck, 0, 0));
        state.set_device_identity("00:00:00:00:00:01", "BlueZ name");
        state.begin_session(1);
        state.set_device_identity("00:00:00:00:00:01", "");
        assert_eq!(state.device_name, "BlueZ name");
        state.begin_session(1);
        let metadata = ParsedPacket::Metadata {
            name: "Pods".into(),
            model_number: "A3048".into(),
            manufacturer: "Apple".into(),
        };
        assert!(state.apply_packet(&metadata, 1, 1));
        assert_eq!(state.model, AirPodsModel::AirPodsPro2Usbc);
        reduce(&mut state, &[4, 0, 4, 0, 6, 0, 2, 9], 2);
        assert_eq!(
            state.wear,
            Some(WearReport {
                primary: WearState::InCase,
                secondary: WearState::Unknown(9)
            })
        );
        assert!(!state.primary_in_ear);
        assert!(!state.secondary_in_ear);
        assert!(state.apply_packet(
            &ParsedPacket::EarDetection {
                primary_in_ear: true,
                secondary_in_ear: false
            },
            1,
            3
        ));
        assert_eq!(state.wear.expect("legacy report").primary, WearState::InEar);
        assert!(state.apply_packet(
            &ParsedPacket::EarDetection {
                primary_in_ear: false,
                secondary_in_ear: true
            },
            1,
            4
        ));
        for (id, value) in [
            (ControlCommandId::ConversationDetectConfig, 1),
            (ControlCommandId::OneBudAncMode, 2),
            (ControlCommandId::AutoAncStrength, 40),
            (ControlCommandId::VoiceTrigger, 9),
        ] {
            assert!(state.apply_packet(
                &ParsedPacket::ControlCommand {
                    id,
                    data: [value, 0, 0, 0]
                },
                1,
                5
            ));
        }
        assert!(state.conversational_awareness);
        assert!(!state.one_bud_anc);
        assert_eq!(state.adaptive_noise_level, 40);
        reduce(&mut state, &ControlCommand::create(0x3e, &[1, 2, 3, 4]), 6);
        assert_eq!(state.reported_controls[&0x3e], [1, 2, 3, 4]);
        let keys = ParsedPacket::ProximityKeys(vec![
            ProximityKey {
                key_type: ProximityKeyType::Irk,
                data: vec![1; 16],
            },
            ProximityKey {
                key_type: ProximityKeyType::EncKey,
                data: vec![2; 16],
            },
            ProximityKey {
                key_type: ProximityKeyType::Irk,
                data: vec![],
            },
        ]);
        assert!(state.apply_packet(&keys, 1, 7));
        assert_eq!(state.magic_acc_irk, Some([1; 16]));
        assert!(state.apply_packet(
            &ParsedPacket::MagicCloudKeys {
                irk: [3; 16],
                enc_key: [4; 16]
            },
            1,
            8
        ));
        assert_eq!(state.magic_acc_enc_key, Some([4; 16]));
        assert!(!format!("{state:?}").contains("3, 3, 3"));
        for packet in [
            ParsedPacket::HandshakeAck,
            ParsedPacket::FeaturesAck,
            ParsedPacket::Unknown(vec![]),
            ParsedPacket::HeadTracking(vec![]),
        ] {
            assert!(!state.apply_packet(&packet, 1, 9));
        }
        assert!(state.apply_packet(&ParsedPacket::ConversationalAwareness { status: 1 }, 1, 9));
        assert_eq!(state.ca_event, Some(1));
        let revision = state.revision;
        state.set_device_identity("00:00:00:00:00:02", "Other");
        assert!(state.revision > revision);
        assert!(state.magic_acc_irk.is_none());
        assert_eq!(state.model, AirPodsModel::Unknown);
        state.reset();
        assert!(!state.connected);
        assert!(!state.any_pod_in_ear());
        assert!(!state.both_pods_in_ear());
    }
    #[test]
    fn capability_map_is_conservative_and_model_identity_stable() {
        let models = [
            ("A1523", AirPodsModel::AirPods1),
            ("A2031", AirPodsModel::AirPods2),
            ("A2564", AirPodsModel::AirPods3),
            ("A2083", AirPodsModel::AirPodsPro),
            ("A2698", AirPodsModel::AirPodsPro2Lightning),
            ("A3048", AirPodsModel::AirPodsPro2Usbc),
            ("A2096", AirPodsModel::AirPodsMaxLightning),
            ("A3184", AirPodsModel::AirPodsMaxUsbc),
            ("A3050", AirPodsModel::AirPods4),
            ("A3055", AirPodsModel::AirPods4Anc),
            ("A3064", AirPodsModel::AirPodsPro3),
            ("A3454", AirPodsModel::AirPodsMax2),
        ];
        for (index, (number, model)) in models.iter().enumerate() {
            assert_eq!(model_from_number(number), *model);
            assert!(!model.display_name().is_empty());
            assert_eq!(model.legacy_index(), (index + 1) as u8);
        }
        assert_eq!(AirPodsModel::Unknown.display_name(), "");
        assert_eq!(
            AirPodsModel::Unknown.capabilities(),
            ModelCapabilities::default()
        );
        assert!(!AirPodsModel::AirPodsPro3.capabilities().noise_off);
        assert!(AirPodsModel::AirPodsMax2.capabilities().adaptive);
        assert!(!AirPodsModel::AirPodsMax2.capabilities().one_bud_anc);
        for (id, model) in [
            (0x0220, AirPodsModel::AirPods1),
            (0x0f20, AirPodsModel::AirPods2),
            (0x1320, AirPodsModel::AirPods3),
            (0x0e20, AirPodsModel::AirPodsPro),
            (0x1420, AirPodsModel::AirPodsPro2Lightning),
            (0x2420, AirPodsModel::AirPodsPro2Usbc),
            (0x0a20, AirPodsModel::AirPodsMaxLightning),
            (0x1f20, AirPodsModel::AirPodsMaxUsbc),
            (0x1920, AirPodsModel::AirPods4),
            (0x1b20, AirPodsModel::AirPods4Anc),
        ] {
            assert_eq!(model_from_ble_id(id), model);
        }
        assert_eq!(WearState::from_byte(1).in_ear(), Some(false));
        assert_eq!(WearState::Unknown(4).in_ear(), None);
        let state = DeviceState {
            primary_in_ear: true,
            secondary_in_ear: true,
            ..DeviceState::default()
        };
        assert!(state.any_pod_in_ear());
        assert!(state.both_pods_in_ear());
    }
}

#[cfg(test)]
mod ble_reducer_tests {
    use super::*;
    use crate::device::battery::{BatteryStatus, Component};
    use crate::device::ble_advert::parse_advertisement;
    fn advert() -> BleAdvertData {
        let raw = [7, 25, 1, 0x24, 0x20, 0x20, 0x65, 0x08, 0, 0, 4];
        parse_advertisement(&raw).expect("proximity fixture")
    }
    #[test]
    fn wear_orientation_is_report_scoped_and_never_inferred_from_another_source() {
        let mut state = DeviceState::default();
        state.begin_session(1);
        let mut advertisement = advert();
        advertisement.primary_left = true;
        assert!(state.apply_verified_ble(&advertisement, 1, 10, 5000));
        assert_eq!(state.wear_primary_side, Some(Component::Left));
        assert_eq!(state.aacp_primary_side, None);
        let wear = ParsedPacket::EarStatus {
            primary: WearState::InEar,
            secondary: WearState::OutOfEar,
        };
        state.apply_packet(&wear, 1, 11);
        assert_eq!(state.wear_primary_side, None);
        let right = crate::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 2, 1, 90, 2, 1]).unwrap();
        state.apply_packet(&right, 1, 12);
        assert_eq!(state.aacp_primary_side, Some(Component::Right));
        assert_eq!(state.wear_primary_side, None); // A later battery frame cannot orient the earlier report.
        state.apply_packet(&wear, 1, 13);
        assert_eq!(state.wear_primary_side, None);
        let left = crate::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 4, 1, 80, 2, 1]).unwrap();
        state.apply_packet(&left, 1, 14);
        assert_eq!(state.aacp_primary_side, Some(Component::Left));
        assert_eq!(state.wear_primary_side, None);
        state.apply_verified_ble(&advertisement, 1, 15, 5000);
        assert_eq!(state.wear_primary_side, None);
        state.apply_verified_ble(&advertisement, 1, 6000, 5000);
        assert_eq!(state.wear_primary_side, Some(Component::Left));
        state.apply_packet(&right, 1, 6001);
        assert_eq!(state.wear_primary_side, Some(Component::Left));
        state.apply_packet(&wear, 1, 6002);
        assert_eq!(state.wear_primary_side, None);
        state.end_session(1);
        assert_eq!(state.wear_primary_side, None);
        assert_eq!(state.aacp_primary_side, None);
        state.begin_session(2);
        state.apply_packet(&wear, 2, 0);
        assert_eq!(state.wear_primary_side, None);
    }
    #[test]
    fn case_only_battery_updates_do_not_establish_or_reset_aacp_orientation() {
        let mut state = DeviceState::default();
        state.begin_session(1);
        let case = crate::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 8, 1, 80, 2, 1]).unwrap();
        state.apply_packet(&case, 1, 0);
        assert_eq!(state.aacp_primary_side, None);
        let right = crate::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 2, 1, 90, 2, 1]).unwrap();
        state.apply_packet(&right, 1, 1);
        state.apply_packet(&case, 1, 2);
        assert_eq!(state.aacp_primary_side, Some(Component::Right));
        state.apply_packet(
            &ParsedPacket::EarDetection {
                primary_in_ear: true,
                secondary_in_ear: false,
            },
            1,
            3,
        );
        assert_eq!(state.wear_primary_side, None);
    }
    #[test]
    fn verified_ble_enriches_disconnected_state_without_readying_control() {
        let mut state = DeviceState::default();
        state.begin_session(1);
        state.end_session(1);
        let mut advert = advert();
        advert.left_in_ear = true;
        advert.right_in_ear = false;
        advert.primary_left = true;
        advert.lid_state = LidState::Open;
        assert!(state.apply_verified_ble(&advert, 1, 10, 5000));
        assert!(!state.connected);
        assert!(!state.noise_control_reported);
        assert!(!state.ear_detection_reported);
        assert_eq!(state.battery.left.level, 50);
        assert_eq!(state.battery.right.level, 60);
        assert_eq!(state.battery.case_.level, 80);
        assert_eq!(state.battery.primary_side(), Some(Component::Left));
        assert_eq!(state.wear.expect("wear").primary, WearState::InEar);
        assert_eq!(state.lid_state, LidState::Open);
        assert_eq!(
            state.observations["battery.left"].source,
            ObservationSource::Ble
        );
        assert!(!state.apply_verified_ble(&advert, 0, 11, 5000));
    }
    #[test]
    fn newer_connected_aacp_wins_and_stale_ble_cannot_overwrite() {
        let mut state = DeviceState::default();
        state.begin_session(1);
        let parsed = crate::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 4, 1, 99, 2, 1])
            .expect("battery");
        state.apply_packet(&parsed, 1, 100);
        state.apply_packet(
            &ParsedPacket::EarStatus {
                primary: WearState::InEar,
                secondary: WearState::InEar,
            },
            1,
            100,
        );
        let mut advert = advert();
        advert.left_in_ear = false;
        advert.case_battery = None;
        advert.right_battery = None;
        advert.encrypted_left_battery = Some(20);
        state.apply_verified_ble(&advert, 1, 101, 5000);
        assert_eq!(state.battery.left.level, 99);
        assert_eq!(state.wear.expect("wear").primary, WearState::InEar);
        state.apply_verified_ble(&advert, 1, 5101, 5000);
        assert_eq!(state.battery.left.level, 20);
        advert.encrypted_left_battery = Some(10);
        assert!(!state.apply_verified_ble(&advert, 1, 5000, 5000));
        assert_eq!(state.battery.left.level, 20);
        advert.encrypted_left_battery = Some(255);
        state.apply_verified_ble(&advert, 1, 5200, 5000);
        assert_eq!(state.battery.left.level, 20);
    }
    #[test]
    fn unknown_case_zero_and_headset_source_are_not_misrepresented() {
        let mut state = DeviceState::default();
        state.begin_session(1);
        let mut advert = advert();
        advert.case_battery = Some(0);
        advert.both_pods_in_case = false;
        advert.one_pod_in_case = false;
        state.apply_verified_ble(&advert, 1, 1, 100);
        assert!(!state.battery.present.contains(&Component::Case));
        advert.both_pods_in_case = true;
        state.apply_verified_ble(&advert, 1, 2, 100);
        assert_eq!(state.battery.case_.status, BatteryStatus::Discharging);
        assert_eq!(state.wear.expect("case").primary, WearState::InCase);
        state.model = AirPodsModel::AirPodsMaxUsbc;
        advert.primary_left = false;
        advert.encrypted_left_battery = Some(42);
        state.apply_verified_ble(&advert, 1, 3, 100);
        assert_eq!(state.battery.headset.level, 42);
        assert_eq!(state.wear_primary_side, None);
    }
}
