// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{DaemonError, ErrorCode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EarPolicy {
    #[default]
    Off,
    OneRemoved,
    BothRemoved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub address: Option<String>,
    pub command_timeout_ms: u64,
    pub ble_enabled: bool,
    pub ear_policy: EarPolicy,
    pub ca_ducking: bool,
    pub audio_recovery: bool,
    pub auto_route: bool,
    pub low_battery_notifications: bool,
    pub legacy_compatibility: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            address: None,
            command_timeout_ms: 5000,
            ble_enabled: false,
            ear_policy: EarPolicy::Off,
            ca_ducking: false,
            audio_recovery: false,
            auto_route: false,
            low_battery_notifications: false,
            legacy_compatibility: false,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), DaemonError> {
        if self.schema_version != 1 {
            return Err(DaemonError::new(
                ErrorCode::VersionMismatch,
                "config",
                "unsupported config schema",
            ));
        }
        if !(100..=60_000).contains(&self.command_timeout_ms) {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "config",
                "command_timeout_ms must be between 100 and 60000",
            ));
        }
        if let Some(address) = &self.address {
            let parts: Vec<_> = address.split(':').collect();
            if parts.len() != 6
                || parts
                    .iter()
                    .any(|part| part.len() != 2 || u8::from_str_radix(part, 16).is_err())
            {
                return Err(DaemonError::new(
                    ErrorCode::InvalidInput,
                    "config",
                    "invalid Bluetooth address",
                ));
            }
        }
        if self.ble_enabled && !cfg!(feature = "ble") {
            return Err(DaemonError::new(
                ErrorCode::Unsupported,
                "config",
                "BLE support was not compiled; rebuild with --features ble",
            ));
        }
        Ok(())
    }
}

pub fn parse(bytes: &[u8]) -> Result<Config, DaemonError> {
    if bytes.len() > 16 * 1024 {
        return Err(DaemonError::new(
            ErrorCode::InvalidInput,
            "config",
            "config exceeds 16 KiB",
        ));
    }
    let config: Config = serde_json::from_slice(bytes)
        .map_err(|error| DaemonError::new(ErrorCode::InvalidInput, "config", error.to_string()))?;
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_control_only() {
        let c = parse(b"{}").expect("default");
        assert!(!c.auto_route && !c.audio_recovery && !c.ble_enabled && !c.ca_ducking);
        assert_eq!(c.ear_policy, EarPolicy::Off);
    }
    #[test]
    fn rejects_new_schema_bad_address_and_unknown_fields() {
        for bytes in [
            br#"{"schema_version":2}"#.as_slice(),
            br#"{"address":"bad"}"#,
            br#"{"command_timeout_ms":0}"#,
            br#"{"unknown":true}"#,
        ] {
            assert!(parse(bytes).is_err());
        }
    }
}
