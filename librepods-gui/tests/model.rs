// SPDX-License-Identifier: AGPL-3.0-only
use librepods_api::{config::EarPolicy, ipc::Operation};
use librepods_core::device::{
    battery::BatteryStatus,
    state::{AirPodsModel, NoiseControlMode as Mode},
};
use librepods_gui::{demo, model::*};
use serde_json::json;
fn panel() -> PanelModel {
    let mut p = PanelModel::default();
    let s = demo::snapshot();
    let c = s.capabilities();
    p.observe(s, c, 100).unwrap();
    p
}
#[test]
fn controls_require_recent_daemon_and_fresh_supported_metadata() {
    let mut p = panel();
    assert!(p.can(Action::Mode(Mode::Adaptive), 100));
    assert!(!p.can(Action::Mode(Mode::Adaptive), 3700));
    p.pending = true;
    assert!(!p.can(Action::Mode(Mode::Adaptive), 100));
    p.pending = false;
    p.unavailable("socket lost".into());
    assert!(!p.can(Action::Mode(Mode::Adaptive), 100));
    let mut s = demo::snapshot();
    s.device.observations.get_mut("metadata").unwrap().stale = true;
    assert!(validate_action(&s, &s.capabilities(), Action::Mode(Mode::Off)).is_err());
    s.device.observations.get_mut("metadata").unwrap().stale = false;
    s.device.model = AirPodsModel::Unknown;
    assert!(validate_action(&s, &s.capabilities(), Action::Mode(Mode::Off)).is_err());
    s.device.model = AirPodsModel::AirPodsPro3;
    assert!(validate_action(&s, &s.capabilities(), Action::Mode(Mode::Off)).is_err());
    s.device.model = AirPodsModel::AirPods1;
    assert!(validate_action(&s, &s.capabilities(), Action::Mode(Mode::Adaptive)).is_err());
}
#[test]
fn mutation_is_confirmed_without_optimistically_overwriting_reported_state() {
    let mut p = panel();
    let cmd = p.prepare(Action::Mode(Mode::Transparency), 100).unwrap();
    assert!(p.pending);
    assert_eq!(
        p.snapshot.as_ref().unwrap().device.listening_mode,
        Some(Mode::NoiseCancellation)
    );
    assert!(p.prepare(Action::Connect, 100).is_err());
    assert!(
        matches!(cmd.operation(),Operation::ForDevice{operation,..} if matches!(*operation,Operation::Change(_)))
    );
    p.finish(Err("Outcome unknown; inspect state".into()));
    assert!(!p.pending);
    assert!(p.failure.is_some());
    assert!(p.message.is_none());
    p.finish(Ok(()));
    assert!(p.failure.is_none());
    assert!(p.message.is_none());
}
#[test]
fn fresh_generation_is_required_and_session_changes_cancel_prepared_commands() {
    let mut p = panel();
    let cmd = p.prepare(Action::Conversation(true), 100).unwrap();
    let mut s = demo::snapshot();
    s.session_generation += 1;
    assert!(cmd.validate(&s, &s.capabilities()).is_err());
    assert_eq!(toggle(&s, 0x28), None);
    assert_eq!(mode(&s), None);
    s = demo::snapshot();
    s.daemon_instance = "restarted".into();
    assert!(cmd.validate(&s, &s.capabilities()).is_err());
    s = demo::snapshot();
    s.device.address = "11:22:33:44:55:66".into();
    assert!(cmd.validate(&s, &s.capabilities()).is_err());
}
#[test]
fn toggles_reject_unknown_values_and_unobserved_controls() {
    let mut s = demo::snapshot();
    let caps = s.capabilities();
    for id in [0x28, 0x1b] {
        s.device.reported_controls.insert(id, [3, 0, 0, 0]);
        assert_eq!(toggle(&s, id), None);
    }
    assert!(validate_action(&s, &caps, Action::Conversation(true)).is_err());
    assert!(validate_action(&s, &caps, Action::OneBud(true)).is_err());
    s.device.reported_controls.remove(&0x28);
    assert_eq!(control(&s, 0x28), None);
    s = demo::snapshot();
    s.connected = false;
    assert_eq!(toggle(&s, 0x28), None);
    assert_eq!(mode(&s), None);
    assert!(validate_action(&s, &caps, Action::Mode(Mode::Transparency)).is_err());
}
#[test]
fn adaptive_level_requires_adaptive_mode_and_valid_readback() {
    let mut s = demo::snapshot();
    let caps = s.capabilities();
    assert!(validate_action(&s, &caps, Action::Adaptive(50)).is_err());
    s.device.listening_mode = Some(Mode::Adaptive);
    for v in [0, 50, 100] {
        assert!(validate_action(&s, &caps, Action::Adaptive(v)).is_ok());
    }
    assert!(validate_action(&s, &caps, Action::Adaptive(101)).is_err());
    s.device.reported_controls.insert(0x2e, [255, 0, 0, 0]);
    assert!(validate_action(&s, &caps, Action::Adaptive(50)).is_err());
}
#[test]
fn old_daemons_disable_device_management_and_link_actions_follow_bluez() {
    let mut s = demo::snapshot();
    let old = json!({});
    for a in [Action::Connect, Action::Disconnect, Action::Forget] {
        assert!(validate_action(&s, &old, a).is_err());
    }
    let caps = s.capabilities();
    assert!(validate_action(&s, &caps, Action::Disconnect).is_ok());
    assert!(validate_action(&s, &caps, Action::Connect).is_err());
    s.bluez_connected = false;
    assert!(validate_action(&s, &caps, Action::Connect).is_ok());
    assert!(validate_action(&s, &caps, Action::Disconnect).is_err());
    s.adapter_ready = false;
    assert!(validate_action(&s, &caps, Action::Forget).is_err());
    s.device.address.clear();
    assert!(validate_action(&s, &caps, Action::EarPolicy(EarPolicy::Off)).is_err());
}
#[test]
fn forgetting_requires_current_explicit_target_confirmation() {
    let mut p = panel();
    assert!(p.prepare(Action::Forget, 100).is_err());
    p.confirm_forget(100);
    assert!(p.forget_target.is_some());
    let cmd = p.prepare(Action::Forget, 100).unwrap();
    assert!(
        matches!(cmd.operation(),Operation::ForDevice{operation,..} if matches!(*operation,Operation::DeviceForget{confirmed:true}))
    );
    p.finish(Ok(()));
    p.confirm_forget(100);
    let mut s = demo::snapshot();
    s.session_generation += 1;
    let caps = s.capabilities();
    p.observe(s, caps, 100).unwrap();
    assert!(p.forget_target.is_none());
    p.confirm_forget(5000);
    assert!(p.forget_target.is_none());
    p.confirm_forget(100);
    p.unavailable("gone".into());
    assert!(p.forget_target.is_none());
}
#[test]
fn reconnect_clears_connection_error_but_preserves_action_error() {
    let mut p = panel();
    p.finish(Err("unknown outcome".into()));
    p.unavailable("daemon stopped".into());
    let s = demo::snapshot();
    let caps = s.capabilities();
    p.observe(s, caps, 200).unwrap();
    assert!(p.connection_error.is_none());
    assert_eq!(p.failure.as_deref(), Some("unknown outcome"));
    assert!(p.current(200));
    let mut s = demo::snapshot();
    s.revision = 10;
    let caps = s.capabilities();
    p.observe(s, caps, 200).unwrap();
    let s = demo::snapshot();
    let caps = s.capabilities();
    assert!(p.observe(s, caps, 201).is_err());
    assert!(!p.current(201));
    let mut s = demo::snapshot();
    s.schema_version = 2;
    let caps = s.capabilities();
    assert!(p.observe(s, caps, 202).is_err());
}
#[test]
fn battery_distinguishes_zero_unknown_charging_stale_and_invalid() {
    let mut s = demo::snapshot();
    let mut b = s.device.battery.left;
    b.level = 0;
    let v = battery(&s, "Left", "battery.left", b, true);
    assert_eq!(v.percent, Some(0));
    assert!(!v.stale);
    b.status = BatteryStatus::Disconnected;
    assert_eq!(battery(&s, "Left", "battery.left", b, true).percent, None);
    b.status = BatteryStatus::Unknown(3);
    assert_eq!(battery(&s, "Left", "battery.left", b, true).percent, None);
    b.status = BatteryStatus::Charging;
    assert_eq!(
        battery(&s, "Left", "battery.left", b, true).detail,
        "Charging"
    );
    b.status = BatteryStatus::OptimizedCharging;
    assert_eq!(
        battery(&s, "Left", "battery.left", b, true).detail,
        "Charge paused"
    );
    assert!(battery(&s, "Left", "battery.left", b, false).stale);
    s.device.observations.get_mut("battery.left").unwrap().stale = true;
    assert_eq!(
        battery(&s, "Left", "battery.left", b, true).detail,
        "Last reported"
    );
    s.device.observations.clear();
    assert_eq!(battery(&s, "Left", "battery.left", b, true).percent, None);
    b.level = 255;
    assert_eq!(battery(&s, "Left", "battery.left", b, true).percent, None);
}
#[test]
fn all_actions_are_targeted_and_demo_is_local() {
    let mut s = demo::snapshot();
    let actions = [
        Action::Mode(Mode::Adaptive),
        Action::Conversation(true),
        Action::OneBud(false),
        Action::Adaptive(20),
        Action::EarPolicy(EarPolicy::BothRemoved),
        Action::Disconnect,
        Action::Connect,
        Action::Forget,
    ];
    for a in actions {
        let cmd = Command {
            target: Target::from_snapshot(&s),
            action: a,
        };
        assert!(matches!(cmd.operation(), Operation::ForDevice { .. }));
        demo::apply(&mut s, a);
    }
    assert!(s.device.address.is_empty());
    assert_eq!(s.revision, 8);
}

#[test]
fn hardware_labels_cover_the_entire_supported_model_table() {
    use librepods_core::device::state::AirPodsModel::*;
    for m in [
        Unknown,
        AirPods1,
        AirPods2,
        AirPods3,
        AirPods4,
        AirPods4Anc,
        AirPodsPro,
        AirPodsPro2Lightning,
        AirPodsPro2Usbc,
        AirPodsPro3,
        AirPodsMaxLightning,
        AirPodsMaxUsbc,
        AirPodsMax2,
    ] {
        assert!(model_name(m).contains("AirPods"));
    }
}
#[test]
fn empty_panel_is_unavailable_and_boolean_reports_are_exact() {
    let mut p = PanelModel::default();
    assert!(!p.current(0));
    assert!(p.prepare(Action::Connect, 0).is_err());
    p.confirm_forget(0);
    assert!(p.forget_target.is_none());
    let mut s = demo::snapshot();
    s.device.reported_controls.insert(0x28, [1, 0, 0, 0]);
    assert_eq!(toggle(&s, 0x28), Some(true));
    s.device.observations.remove("control.28");
    assert_eq!(control(&s, 0x28), None);
    s.schema_version = 2;
    assert!(validate_action(&s, &s.capabilities(), Action::Disconnect).is_err());
}

#[test]
fn retained_case_history_is_labelled_and_aged_without_inventing_a_percentage() {
    use librepods_core::device::state::DeviceState;
    let mut state = DeviceState::default();
    state.begin_session(1);
    let known =
        librepods_core::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 8, 1, 60, 2, 1]).unwrap();
    state.apply_packet(&known, 1, 10);
    let unavailable =
        librepods_core::protocol::parser::parse(&[4, 0, 4, 0, 4, 0, 1, 8, 1, 0, 4, 1]).unwrap();
    state.apply_packet(&unavailable, 1, 20);
    let mut s = librepods_api::snapshot::Snapshot::new("test".into(), &state);
    s.observation_unix_ms.insert("battery.case".into(), 1000);
    let b = battery(&s, "Case", "battery.case", s.device.battery.case_, true);
    assert_eq!(b.percent, Some(60));
    assert!(b.stale);
    assert_eq!(b.detail_at(1500), "Last reported · just now");
    assert_eq!(b.detail_at(121000), "Last reported · 2m ago");
    assert_eq!(b.detail_at(7201000), "Last reported · 2h ago");
    assert_eq!(b.detail_at(172801000), "Last reported · 2d ago");
    assert_eq!(b.detail_at(500), "Last reported");
    let empty = librepods_api::snapshot::Snapshot::new("empty".into(), &DeviceState::default());
    let b = battery(
        &empty,
        "Case",
        "battery.case",
        empty.device.battery.case_,
        true,
    );
    assert_eq!(b.percent, None);
    assert_eq!(b.detail_at(1000), "Not reported");
}
