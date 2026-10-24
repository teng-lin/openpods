// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = librepods_core::device::ble_advert::parse_advertisement(data);
    if data.len() >= 38 {
        let key: [u8; 16] = data[..16].try_into().expect("checked key length");
        let payload: [u8; 16] = data[16..32].try_into().expect("checked payload length");
        let address: [u8; 6] = data[32..38].try_into().expect("checked address length");
        let _ = librepods_core::device::ble_advert::verify_rpa(&key, &address);
        if let Some(mut advert) =
            librepods_core::device::ble_advert::parse_advertisement(&data[38..])
        {
            librepods_core::device::ble_advert::decrypt_battery(
                &mut advert,
                &payload,
                &key,
                data[0] & 1 == 0,
            );
        }
    }
});
