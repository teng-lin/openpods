// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{config::EarPolicy, DaemonError};
use librepods_core::device::battery::{BatteryInfo, BatteryState, BatteryStatus};
use librepods_core::device::state::{AirPodsModel, DeviceState, NoiseControlMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

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
    pub updated_unix_ms: u64,
    pub observed_monotonic_ms: u64,
    pub device: DeviceStatus,
    pub ear_policy: EarPolicy,
    pub retry_count: u32,
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
            session_generation: 0,
            lifecycle: "waiting_for_adapter".into(),
            connected: false,
            updated_unix_ms: 0,
            observed_monotonic_ms: 0,
            device: DeviceStatus::from(state),
            ear_policy: EarPolicy::Off,
            retry_count: 0,
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

    pub fn legacy(&self) -> Value {
        let d = &self.device;
        let capabilities = self.capabilities();
        let pod = |reading: BatteryState, in_ear: Option<bool>| {
            json!({
            "available":available(reading),"level":reading.level,
            "charging":matches!(reading.status,BatteryStatus::Charging),
            "in_ear":in_ear.unwrap_or(false)})
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
            "left":pod(d.battery.left,match d.battery.primary_side() {Some(librepods_core::device::battery::Component::Left)=>d.primary_in_ear,Some(librepods_core::device::battery::Component::Right)=>d.secondary_in_ear,_=>None}),
            "right":pod(d.battery.right,match d.battery.primary_side() {Some(librepods_core::device::battery::Component::Left)=>d.secondary_in_ear,Some(librepods_core::device::battery::Component::Right)=>d.primary_in_ear,_=>None}),
            "case":{"available":available(d.battery.case_),"level":d.battery.case_.level,
                "charging":matches!(d.battery.case_.status,BatteryStatus::Charging)},
            "headset":{"available":available(d.battery.headset),"level":d.battery.headset.level,"charging":matches!(d.battery.headset.status,BatteryStatus::Charging)},
            "one_bud_anc_mode":toggle(0x1B),"conversational_awareness":toggle(0x28),
            "adaptive_noise_level":d.reported_controls.get(&0x2E).map(|d|d[0]).unwrap_or(50),
            "ear_detection_behavior":match self.ear_policy {EarPolicy::OneRemoved=>0,EarPolicy::BothRemoved=>1,EarPolicy::Off=>2},
            "lid_state":d.lid_state.unwrap_or(2),"daemon_instance":self.daemon_instance,"revision":self.revision,
            "reconnect_attempts_total":self.retry_count})
    }

    pub fn capabilities(&self) -> Value {
        let capabilities = self.device.model.capabilities();
        json!({"noise_control":capabilities.noise_control,"noise_off":capabilities.noise_off,
            "adaptive":capabilities.adaptive,"conversational_awareness":capabilities.conversational_awareness,
            "one_bud_anc":capabilities.one_bud_anc,"ble":cfg!(feature="ble")})
    }
}

pub fn available(reading: BatteryState) -> bool {
    matches!(
        reading.status,
        BatteryStatus::Charging | BatteryStatus::Discharging | BatteryStatus::OptimizedCharging
    )
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
            primary_in_ear: state.ear_detection_reported.then_some(state.primary_in_ear),
            secondary_in_ear: state
                .ear_detection_reported
                .then_some(state.secondary_in_ear),
            reported_controls: state.reported_controls.clone(),
            wear: state.wear,
            observations: state.observations.clone(),
            ca_event: None,
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
