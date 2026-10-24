// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Session-owned optional telemetry. Secrets never cross the public IPC boundary.

use std::time::{Duration, Instant};

use librepods_core::{
    device::state::DeviceState,
    protocol::parser::{ParsedPacket, ProximityKeyType},
};
use serde_json::{json, Value};

use crate::{
    ble::{BleCoordinator, BleEvent},
    keys::{BleKeys, KeyStore},
    storage::Paths,
    DaemonError, ErrorCode,
};

pub(crate) struct Telemetry {
    origin: Instant,
    paths: Option<Paths>,
    coordinator: Option<BleCoordinator>,
    keys: Option<BleKeys>,
    address: String,
    error: Option<DaemonError>,
}

impl Telemetry {
    pub fn new(origin: Instant, paths: Option<Paths>) -> Self {
        Self {
            origin,
            paths,
            coordinator: None,
            keys: None,
            address: String::new(),
            error: None,
        }
    }

    pub fn pause_for_connection(&self) -> Result<(), DaemonError> {
        match &self.coordinator {
            Some(coordinator) => coordinator.pause_for_connection(Duration::from_millis(500)),
            None => Ok(()),
        }
    }

    fn store(&self) -> Result<KeyStore, DaemonError> {
        let paths = self.paths.as_ref().ok_or_else(|| {
            DaemonError::new(
                ErrorCode::Unsupported,
                "BLE keys",
                "key persistence requires the persistent service",
            )
        })?;
        KeyStore::load(paths.state.join("ble-keys.json"))
    }

    pub fn forget(&mut self, address: &str) -> Result<(), DaemonError> {
        self.store()?.remove(address)?;
        self.keys = None;
        // Immediately invalidate queued advertisements under the old key revision.
        if let Some(scanner) = &self.coordinator {
            scanner.configure(false, false, 0, None)?;
        }
        self.error = None;
        Ok(())
    }

    pub fn persist(&mut self, address: &str, keys: &KeyRead) -> Result<(), DaemonError> {
        let (Some(irk), Some(enc)) = (keys.irk, keys.enc) else {
            return Err(DaemonError::new(
                ErrorCode::MalformedFrame,
                "BLE keys",
                "both requested keys were not received",
            ));
        };
        let keys = BleKeys::new(address, irk, enc)?;
        self.store()?.save(&keys)?;
        self.address = address.to_owned();
        self.keys = Some(keys);
        self.error = None;
        Ok(())
    }

    /// The caller supplies shared-controller policy. Connected/recovering/suspended
    /// sessions never scan until a separate coexistence acceptance run permits it.
    pub fn poll(
        &mut self,
        state: &mut DeviceState,
        generation: u64,
        enabled: bool,
        allowed: bool,
    ) -> Value {
        if enabled && self.address != state.bluetooth_address && !state.bluetooth_address.is_empty()
        {
            self.address.clone_from(&state.bluetooth_address);
            match self.store().and_then(|store| store.get(&self.address)) {
                Ok(keys) => {
                    self.keys = keys;
                    self.error = None;
                }
                Err(error) => {
                    self.keys = None;
                    self.error = Some(error);
                }
            }
        }
        if enabled && self.coordinator.is_none() {
            match BleCoordinator::start(self.origin) {
                Ok(coordinator) => self.coordinator = Some(coordinator),
                Err(error) => self.error = Some(error),
            }
        }
        if let Some(coordinator) = &self.coordinator {
            if let Err(error) =
                coordinator.configure(enabled, allowed, generation, self.keys.clone())
            {
                self.error = Some(error);
            }
            // Bound processing even under a noisy controller; commands retain priority.
            for _ in 0..32 {
                match coordinator.next_event_timeout(Duration::ZERO) {
                    Ok(Some(BleEvent::Observation {
                        generation: observed_generation,
                        observed_at_ms,
                        advertisement,
                    })) if observed_generation == generation => {
                        state.apply_verified_ble(
                            &advertisement,
                            generation,
                            observed_at_ms,
                            30_000,
                        );
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => {
                        self.error = Some(error);
                        break;
                    }
                }
            }
        }
        json!({
            "compiled": cfg!(feature="ble"), "enabled": enabled,
            "health": self.coordinator.as_ref().map(|scanner| scanner.health()).unwrap_or_default(),
            "keys_available": self.keys.is_some(), "last_error": self.error.clone().or_else(|| self.coordinator.as_ref().and_then(|scanner| scanner.last_error())),
            "connected_scanning": false,
        })
    }
}

/// Only packets received after the explicit request contribute to this exchange.
/// Cached reducer keys can never satisfy a new acquisition request.
#[derive(Default)]
pub(crate) struct KeyRead {
    irk: Option<[u8; 16]>,
    enc: Option<[u8; 16]>,
}

impl KeyRead {
    pub fn observe(&mut self, packet: &ParsedPacket) -> bool {
        match packet {
            ParsedPacket::MagicCloudKeys { irk, enc_key } => {
                self.irk = Some(*irk);
                self.enc = Some(*enc_key);
            }
            ParsedPacket::ProximityKeys(keys) => {
                for key in keys {
                    if let Ok(bytes) = <[u8; 16]>::try_from(key.data.as_slice()) {
                        match key.key_type {
                            ProximityKeyType::Irk => self.irk = Some(bytes),
                            ProximityKeyType::EncKey => self.enc = Some(bytes),
                        }
                    }
                }
            }
            _ => {}
        }
        self.irk.is_some() && self.enc.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librepods_core::protocol::parser::ProximityKey;

    #[test]
    fn key_exchange_requires_both_new_valid_keys_and_ignores_other_packets() {
        let mut read = KeyRead::default();
        assert!(!read.observe(&ParsedPacket::Unknown(vec![4, 0])));
        let key =
            |key_type, data| ParsedPacket::ProximityKeys(vec![ProximityKey { key_type, data }]);
        assert!(!read.observe(&key(ProximityKeyType::Irk, vec![1; 15])));
        assert!(!read.observe(&key(ProximityKeyType::EncKey, vec![2; 16])));
        assert!(read.observe(&key(ProximityKeyType::Irk, vec![1; 16])));
        assert_eq!(read.irk, Some([1; 16]));
        assert_eq!(read.enc, Some([2; 16]));
    }

    #[test]
    fn no_ble_default_has_no_scanner_and_secrets_never_enter_status() {
        let mut telemetry = Telemetry::new(Instant::now(), None);
        let mut state = DeviceState::default();
        state.set_device_identity("AA:BB:CC:DD:EE:FF", "Example");
        let status = telemetry.poll(&mut state, 0, false, true);
        assert!(telemetry.coordinator.is_none());
        assert_eq!(status["health"], "disabled");
        assert_eq!(
            telemetry.forget(&state.bluetooth_address).unwrap_err().code,
            ErrorCode::Unsupported
        );
    }
}
