// SPDX-License-Identifier: AGPL-3.0-only
//! Synthetic data only; demo mode never creates a daemon client.
use librepods_api::snapshot::Snapshot;
use librepods_core::device::{
    battery::{BatteryState, BatteryStatus},
    state::{AirPodsModel, DeviceState, NoiseControlMode, ObservationMeta, ObservationSource},
};
pub fn snapshot() -> Snapshot {
    let mut s = Snapshot::new("demo".into(), &DeviceState::default());
    s.connected = true;
    s.adapter_ready = true;
    s.bluez_connected = true;
    s.lifecycle = "ready".into();
    s.session_generation = 1;
    s.device.name = "AirPods Pro".into();
    s.device.address = "00:11:22:33:44:55".into();
    s.device.model = AirPodsModel::AirPodsPro2Usbc;
    s.device.model_number = "A3048".into();
    s.device.listening_mode = Some(NoiseControlMode::NoiseCancellation);
    s.device.battery.left = BatteryState {
        level: 86,
        status: BatteryStatus::Discharging,
    };
    s.device.battery.right = BatteryState {
        level: 82,
        status: BatteryStatus::Discharging,
    };
    s.device.battery.case_ = BatteryState {
        level: 64,
        status: BatteryStatus::Charging,
    };
    for (id, val) in [(0x28, 2), (0x1b, 1), (0x2e, 50)] {
        s.device.reported_controls.insert(id, [val, 0, 0, 0]);
    }
    for field in [
        "metadata",
        "noise_control",
        "control.28",
        "control.1b",
        "control.2e",
        "battery.left",
        "battery.right",
        "battery.case",
    ] {
        s.device.observations.insert(
            field.into(),
            ObservationMeta {
                generation: 1,
                observed_at_ms: 0,
                source: ObservationSource::Aacp,
                stale: false,
            },
        );
    }
    s
}
pub fn apply(s: &mut Snapshot, a: crate::model::Action) {
    use crate::model::Action;
    match a {
        Action::Mode(v) => s.device.listening_mode = Some(v),
        Action::Conversation(v) => {
            s.device
                .reported_controls
                .insert(0x28, [if v { 1 } else { 2 }, 0, 0, 0]);
        }
        Action::OneBud(v) => {
            s.device
                .reported_controls
                .insert(0x1b, [if v { 1 } else { 2 }, 0, 0, 0]);
        }
        Action::Adaptive(v) => {
            s.device.reported_controls.insert(0x2e, [v, 0, 0, 0]);
        }
        Action::EarPolicy(v) => s.ear_policy = v,
        Action::Connect => {
            s.bluez_connected = true;
            s.connected = true;
            s.connection_desired = true;
            s.lifecycle = "ready".into();
            for meta in s.device.observations.values_mut() {
                meta.stale = false;
            }
        }
        Action::Disconnect => {
            s.bluez_connected = false;
            s.connected = false;
            s.connection_desired = false;
            s.lifecycle = "disconnected".into();
            for meta in s.device.observations.values_mut() {
                meta.stale = true;
            }
        }
        Action::Forget => {
            s.bluez_connected = false;
            s.connected = false;
            s.connection_desired = false;
            s.lifecycle = "disconnected".into();
            for meta in s.device.observations.values_mut() {
                meta.stale = true;
            }
            s.device.address.clear();
        }
    }
    s.revision += 1;
}
