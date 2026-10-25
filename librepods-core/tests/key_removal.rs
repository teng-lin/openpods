// SPDX-License-Identifier: AGPL-3.0-only
#[test]
fn explicit_key_removal_invalidates_private_reducer_state_and_is_idempotent() {
    use librepods_core::{device::state::DeviceState, protocol::parser::ParsedPacket};
    let mut state = DeviceState::default();
    state.begin_session(1);
    state.apply_packet(
        &ParsedPacket::MagicCloudKeys {
            irk: [1; 16],
            enc_key: [2; 16],
        },
        1,
        0,
    );
    assert!(state.magic_acc_irk.is_some());
    state.forget_proximity_keys();
    assert!(state.magic_acc_irk.is_none());
    assert!(state.magic_acc_enc_key.is_none());
    assert!(!state.observations.contains_key("keys"));
    assert!(
        state.connected,
        "removing BLE keys must not disconnect AACP"
    );
    let revision = state.revision;
    state.forget_proximity_keys();
    assert_eq!(state.revision, revision);
}
