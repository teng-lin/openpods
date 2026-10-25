// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{config::EarPolicy, DaemonError};
use librepods_core::device::battery::{BatteryInfo, BatteryState, BatteryStatus};
use librepods_core::device::state::{AirPodsModel, DeviceState, NoiseControlMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn default_connection_desired() -> bool {
    true
}

/// Public DTO deliberately has no encryption or identity-key fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub name: String,
    pub address: String,
    pub model_number: String,
    pub manufacturer: String,
    pub model: AirPodsModel,
    pub battery: BatteryInfo,
    pub listening_mode: Option<NoiseControlMode>,
    pub primary_in_ear: Option<bool>,
    pub secondary_in_ear: Option<bool>,
    pub reported_controls: BTreeMap<u8, [u8; 4]>,
    pub wear: Option<librepods_core::device::state::WearReport>,
    /// Orientation at wear-report receipt; None preserves unknown absolute side.
    #[serde(default)]
    pub wear_primary_side: Option<librepods_core::device::battery::Component>,
    pub observations: BTreeMap<String, librepods_core::device::state::ObservationMeta>,
    pub ca_event: Option<u8>,
    pub lid_state: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub daemon_instance: String,
    pub revision: u64,
    pub session_generation: u64,
    pub lifecycle: String,
    pub connected: bool,
    /// Readiness of the selected device's adapter, independent of AACP.
    #[serde(default)]
    pub adapter_ready: bool,
    /// Last reconciled BlueZ link state; `connected` denotes AACP readiness.
    #[serde(default)]
    pub bluez_connected: bool,
    /// False after an intentional device disconnect/forget until explicit reconnect.
    #[serde(default = "default_connection_desired")]
    pub connection_desired: bool,
    pub updated_unix_ms: u64,
    pub observed_monotonic_ms: u64,
    /// Receipt times in wall-clock milliseconds, retained through disconnects.
    #[serde(default)]
    pub observation_unix_ms: BTreeMap<String, u64>,
    pub device: DeviceStatus,
    pub ear_policy: EarPolicy,
    pub retry_count: u32,
    #[serde(default)]
    pub reconnect_attempts_total: u64,
    #[serde(default)]
    pub readback_attempts_total: u64,
    pub retry_at_ms: Option<u64>,
    pub queue_depth: usize,
    pub malformed_frames: u64,
    pub last_error: Option<DaemonError>,
    pub publication_error: Option<DaemonError>,
    pub audio: Value,
    pub ble: Value,
    pub ble_enabled: bool,
}

impl Snapshot {
    pub fn new(instance: String, state: &DeviceState) -> Self {
        Self {
            schema_version: 1,
            daemon_instance: instance,
            revision: 0,
            session_generation: state.session_generation,
            lifecycle: "waiting_for_adapter".into(),
            connected: state.connected,
            adapter_ready: false,
            bluez_connected: false,
            connection_desired: true,
            updated_unix_ms: 0,
            observed_monotonic_ms: 0,
            observation_unix_ms: BTreeMap::new(),
            device: DeviceStatus::from(state),
            ear_policy: EarPolicy::Off,
            retry_count: 0,
            reconnect_attempts_total: 0,
            readback_attempts_total: 0,
            retry_at_ms: None,
            queue_depth: 0,
            malformed_frames: 0,
            last_error: None,
            publication_error: None,
            audio: json!({"health":"not_observed"}),
            ble: json!({"compiled":cfg!(feature="ble"),"enabled":false,"health":"disabled"}),
            ble_enabled: false,
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn observe_device(&mut self, state: &DeviceState, monotonic_ms: u64, unix_ms: u64) {
        self.observation_unix_ms
            .retain(|field, _| state.observations.contains_key(field));
        for (field, observation) in &state.observations {
            let same_receipt = self.device.observations.get(field).is_some_and(|previous| {
                previous.generation == observation.generation
                    && previous.observed_at_ms == observation.observed_at_ms
                    && previous.source == observation.source
            });
            if !same_receipt || !self.observation_unix_ms.contains_key(field) {
                self.observation_unix_ms.insert(
                    field.clone(),
                    unix_ms.saturating_sub(monotonic_ms.saturating_sub(observation.observed_at_ms)),
                );
            }
        }
        self.device = DeviceStatus::from(state);
    }

    pub fn legacy(&self) -> Value {
        let d = &self.device;
        let capabilities = self.capabilities();
        let fresh = |field: &str| {
            d.observations.get(field).is_some_and(|observation| {
                !observation.stale && observation.generation == self.session_generation
            })
        };
        let pod = |reading: BatteryState, in_ear: Option<bool>, field: &str| {
            json!({
            "available":available(reading) && fresh(field),"level":reading.level,
            "charging":matches!(reading.status,BatteryStatus::Charging),
            "in_ear":in_ear.unwrap_or(false),"in_ear_known":in_ear.is_some()})
        };
        let absolute_wear = |left: bool| match d.wear_primary_side {
            Some(librepods_core::device::battery::Component::Left) => {
                if left {
                    d.primary_in_ear
                } else {
                    d.secondary_in_ear
                }
            }
            Some(librepods_core::device::battery::Component::Right) => {
                if left {
                    d.secondary_in_ear
                } else {
                    d.primary_in_ear
                }
            }
            _ => match (d.primary_in_ear, d.secondary_in_ear) {
                (Some(primary), Some(secondary)) if primary == secondary => Some(primary),
                _ => None,
            },
        };
        let toggle = |id: u8| {
            d.reported_controls
                .get(&id)
                .is_some_and(|data| data[0] == 1)
        };
        json!({"schema_version":1,"connected":self.connected,"device_name":d.name,
            "model_name":d.model.display_name(),"model_number":d.model_number,
            "is_pro_series":format!("{:?}",d.model).contains("Pro"),
            "is_headset":d.model.capabilities().headset,
            "supports_noise_control":capabilities["noise_control"],
            "supports_noise_off":capabilities["noise_off"],
            "supports_adaptive":capabilities["adaptive"],
            "supports_conversational_awareness":capabilities["conversational_awareness"],
            "supports_one_bud_anc":capabilities["one_bud_anc"],
            "noise_mode":d.listening_mode.map(|m|i32::from(m.as_byte())-1).unwrap_or(-1),
            "left":pod(d.battery.left,absolute_wear(true),"battery.left"),
            "right":pod(d.battery.right,absolute_wear(false),"battery.right"),
            "case":{"available":available(d.battery.case_) && fresh("battery.case"),"level":d.battery.case_.level,
                "charging":matches!(d.battery.case_.status,BatteryStatus::Charging)},
            "headset":{"available":available(d.battery.headset) && fresh("battery.headset"),"level":d.battery.headset.level,"charging":matches!(d.battery.headset.status,BatteryStatus::Charging)},
            "one_bud_anc_mode":toggle(0x1B),"conversational_awareness":toggle(0x28),
            "adaptive_noise_level":d.reported_controls.get(&0x2E).map(|d|d[0]).unwrap_or(50),
            "ear_detection_behavior":match self.ear_policy {EarPolicy::OneRemoved=>0,EarPolicy::BothRemoved=>1,EarPolicy::Off=>2},
            "lid_state":d.lid_state.unwrap_or(2),"daemon_instance":self.daemon_instance,"revision":self.revision,
            "reconnect_attempts_total":self.reconnect_attempts_total})
    }

    pub fn capabilities(&self) -> Value {
        let capabilities = self.device.model.capabilities();
        json!({"noise_control":capabilities.noise_control,"noise_off":capabilities.noise_off,
            "adaptive":capabilities.adaptive,"conversational_awareness":capabilities.conversational_awareness,
            "one_bud_anc":capabilities.one_bud_anc,"device_management":true,"ble":cfg!(feature="ble")})
    }

    pub fn doctor(&self) -> Value {
        json!({
            "lifecycle": self.lifecycle,
            "adapter_ready": self.adapter_ready,
            "bluez_connected": self.bluez_connected,
            "connection_desired": self.connection_desired,
            "aacp_ready": self.connected,
            "last_error": self.last_error,
            "ble": self.ble,
            "queue_depth": self.queue_depth,
            "reconnect_attempts_total": self.reconnect_attempts_total,
            "readback_attempts_total": self.readback_attempts_total,
        })
    }
}

pub fn available(reading: BatteryState) -> bool {
    matches!(
        reading.status,
        BatteryStatus::Charging | BatteryStatus::Discharging | BatteryStatus::OptimizedCharging
    )
}

/// Large audio graphs have an explicit detailed endpoint. They must not prevent
/// battery, command and recovery status from fitting in the native frame budget.
#[cfg(any(target_os = "linux", test))]
pub fn bounded_audio(value: Value) -> Value {
    const DETAILS_LIMIT: usize = 32 * 1024;
    if serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() <= DETAILS_LIMIT) {
        return value;
    }
    let mut summary = serde_json::Map::new();
    for key in [
        "health",
        "server_name",
        "profile",
        "default_output",
        "default_input",
        "capture_active",
        "capture_known",
        "media_history_ready",
        "routing_policy",
        "audio_recovery",
        "automation_yielded",
        "last_recovery",
        "last_error",
    ] {
        if let Some(value) = value.get(key) {
            let value = match value {
                Value::String(text) => Value::String(text.chars().take(1024).collect()),
                Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
                _ => Value::String("details omitted; query audio status".into()),
            };
            summary.insert(key.into(), value);
        }
    }
    for key in [
        "profiles",
        "outputs",
        "inputs",
        "playback_streams",
        "recording_streams",
    ] {
        if let Some(array) = value.get(key).and_then(Value::as_array) {
            summary.insert(format!("{key}_count"), json!(array.len()));
        }
    }
    summary.insert("details_omitted".into(), Value::Bool(true));
    Value::Object(summary)
}

impl From<&DeviceState> for DeviceStatus {
    fn from(state: &DeviceState) -> Self {
        Self {
            name: state.device_name.clone(),
            address: state.bluetooth_address.clone(),
            model_number: state.model_number.clone(),
            manufacturer: state.manufacturer.clone(),
            model: state.model,
            battery: state.battery.clone(),
            listening_mode: state.noise_control_reported.then_some(state.noise_control),
            primary_in_ear: state.wear.and_then(|wear| wear.primary.in_ear()),
            secondary_in_ear: state.wear.and_then(|wear| wear.secondary.in_ear()),
            reported_controls: state.reported_controls.clone(),
            wear: state.wear,
            wear_primary_side: state.wear_primary_side,
            observations: state.observations.clone(),
            ca_event: state.ca_event,
            lid_state: match state.lid_state {
                librepods_core::device::ble_advert::LidState::Open => Some(0),
                librepods_core::device::ble_advert::LidState::Closed => Some(1),
                librepods_core::device::ble_advert::LidState::Unknown => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_audio_graph_keeps_health_and_reports_explicitly_omitted_details() {
        let value = json!({"health":"Healthy","profile":"a2dp-sink","default_input":"external microphone","capture_known":true,"capture_active":false,"outputs":vec!["x".repeat(1024);100]});
        let summary = bounded_audio(value);
        assert_eq!(summary["profile"], "a2dp-sink");
        assert_eq!(summary["default_input"], "external microphone");
        assert_eq!(summary["capture_active"], false);
        assert_eq!(summary["details_omitted"], true);
        assert_eq!(summary["outputs_count"], 100);
        assert!(serde_json::to_vec(&summary).expect("summary").len() < 1024);
        let small = json!({"outputs":[],"health":"MissingSink"});
        assert_eq!(bounded_audio(small.clone()), small);
    }
    #[test]
    fn observation_wall_times_survive_clock_changes_and_stale_publication() {
        let mut state = DeviceState::default();
        state.begin_session(1);
        let packet =
            librepods_core::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 4, 1, 42, 2, 1])
                .expect("battery");
        state.apply_packet(&packet, 1, 10);
        let mut snapshot = Snapshot::new("test".into(), &state);
        snapshot.observe_device(&state, 20, 1000);
        assert_eq!(snapshot.observation_unix_ms["battery.left"], 990);
        snapshot.observe_device(&state, 30, 500);
        assert_eq!(snapshot.observation_unix_ms["battery.left"], 990);
        state.end_session(1);
        snapshot.observe_device(&state, 40, 2000);
        assert_eq!(snapshot.observation_unix_ms["battery.left"], 990);
        state.begin_session(2);
        state.apply_packet(&packet, 2, 50);
        snapshot.observe_device(&state, 60, 3000);
        assert_eq!(snapshot.observation_unix_ms["battery.left"], 2990);
    }
    #[test]
    fn legacy_battery_does_not_present_stale_cached_readings_as_available() {
        let mut state = DeviceState::default();
        state.begin_session(7);
        let packet = librepods_core::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 4, 1, 0, 2, 1])
            .expect("battery");
        state.apply_packet(&packet, 7, 10);
        let fresh = Snapshot::new("test".into(), &state);
        assert_eq!(fresh.legacy()["left"]["available"], true);
        assert_eq!(fresh.legacy()["left"]["level"], 0);
        state.end_session(7);
        let stale = Snapshot::new("test".into(), &state);
        assert_eq!(stale.device.battery.left.level, 0);
        assert_eq!(stale.legacy()["left"]["available"], false);
    }
    #[test]
    fn mixed_source_battery_updates_cannot_relabel_fresh_aacp_wear() {
        use librepods_core::{
            device::{ble_advert::parse_advertisement, state::WearState},
            protocol::parser::{self, ParsedPacket},
        };
        let mut state = DeviceState::default();
        state.begin_session(1);
        let right = parser::parse(&[4, 0, 4, 0, 4, 0, 1, 2, 1, 99, 2, 1]).unwrap();
        state.apply_packet(&right, 1, 100);
        state.apply_packet(
            &ParsedPacket::EarStatus {
                primary: WearState::InEar,
                secondary: WearState::OutOfEar,
            },
            1,
            101,
        );
        let mut advert =
            parse_advertisement(&[7, 25, 1, 0x24, 0x20, 0x20, 0x65, 0x08, 0, 0, 4]).unwrap();
        advert.primary_left = true;
        state.apply_verified_ble(&advert, 1, 102, 5000);
        let snapshot = Snapshot::new("test".into(), &state);
        assert_eq!(snapshot.device.primary_in_ear, Some(true));
        assert_eq!(snapshot.device.wear_primary_side, None);
        assert_eq!(snapshot.legacy()["right"]["in_ear_known"], false);
        assert_eq!(snapshot.legacy()["left"]["in_ear_known"], false);
        assert_eq!(snapshot.legacy()["left"]["in_ear"], false);
    }
    #[test]
    fn verified_ble_wear_is_available_without_aacp_flags_and_unknown_wear_stays_unknown() {
        use librepods_core::{
            device::{battery::Component, ble_advert::parse_advertisement, state::WearState},
            protocol::parser::ParsedPacket,
        };
        let mut state = DeviceState::default();
        state.begin_session(1);
        state.end_session(1);
        let mut advert =
            parse_advertisement(&[7, 25, 1, 0x24, 0x20, 0x20, 0x65, 0x08, 0, 0, 4]).unwrap();
        advert.primary_left = true;
        advert.left_in_ear = true;
        advert.right_in_ear = false;
        advert.both_pods_in_case = false;
        state.apply_verified_ble(&advert, 1, 1, 5000);
        assert!(!state.ear_detection_reported);
        let snapshot = Snapshot::new("test".into(), &state);
        assert_eq!(snapshot.device.wear_primary_side, Some(Component::Left));
        assert_eq!(snapshot.device.primary_in_ear, Some(true));
        assert_eq!(snapshot.legacy()["left"]["in_ear"], true);
        state.begin_session(2);
        state.apply_packet(
            &ParsedPacket::EarStatus {
                primary: WearState::Unknown(7),
                secondary: WearState::OutOfEar,
            },
            2,
            2,
        );
        let snapshot = Snapshot::new("test".into(), &state);
        assert_eq!(snapshot.device.primary_in_ear, None);
        assert_eq!(snapshot.device.wear_primary_side, None);
    }
    #[test]
    fn documented_aacp_role_switch_never_claims_an_unproven_absolute_ear() {
        use librepods_core::{
            device::state::WearState,
            protocol::parser::{self, ParsedPacket},
        };
        let mut state = DeviceState::default();
        state.begin_session(1);
        state.apply_packet(
            &parser::parse(&[4, 0, 4, 0, 4, 0, 1, 2, 1, 99, 2, 1]).unwrap(),
            1,
            0,
        );
        // The protocol permits a microphone/primary switch on removal. This same
        // asymmetric frame can describe either physical ear without a role ID.
        state.apply_packet(
            &ParsedPacket::EarStatus {
                primary: WearState::InEar,
                secondary: WearState::OutOfEar,
            },
            1,
            1,
        );
        let asymmetric = Snapshot::new("test".into(), &state);
        assert_eq!(asymmetric.device.primary_in_ear, Some(true));
        assert_eq!(asymmetric.device.secondary_in_ear, Some(false));
        assert_eq!(asymmetric.device.wear_primary_side, None);
        assert_eq!(asymmetric.legacy()["left"]["in_ear_known"], false);
        assert_eq!(asymmetric.legacy()["right"]["in_ear_known"], false);
        state.apply_packet(
            &ParsedPacket::EarStatus {
                primary: WearState::InEar,
                secondary: WearState::InEar,
            },
            1,
            2,
        );
        let symmetric = Snapshot::new("test".into(), &state);
        for side in ["left", "right"] {
            assert_eq!(symmetric.legacy()[side]["in_ear"], true);
            assert_eq!(symmetric.legacy()[side]["in_ear_known"], true);
        }
    }
    #[test]
    fn public_status_never_exposes_keys_or_invents_connected_measurements() {
        let state = DeviceState {
            magic_acc_irk: Some([42; 16]),
            magic_acc_enc_key: Some([43; 16]),
            ..DeviceState::default()
        };
        let snapshot = Snapshot::new("test".into(), &state);
        let value = serde_json::to_value(&snapshot).expect("json");
        let encoded = value.to_string();
        assert!(!encoded.contains("magic_acc"));
        assert!(!encoded.contains("irk"));
        assert!(value["device"]["listening_mode"].is_null());
        assert_eq!(snapshot.legacy()["noise_mode"], -1);
        assert_eq!(snapshot.legacy()["case"]["available"], false);
    }
}
