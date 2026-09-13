// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Windows media control.
//!
//! - Playback (play/pause) is driven through the System Media Transport
//!   Controls (SMTC) WinRT API, which controls whichever media session is
//!   currently active.
//! - The active output device is checked by querying the default audio render
//!   endpoint through `Windows.Media.Devices`.
//!
//! Windows routes Bluetooth A2DP audio automatically once the AirPods connect,
//! so audio activation is a no-op.

use librepods_core::transport::{MediaControl, TransportError};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
use windows::Media::Devices::{AudioDeviceRole, MediaDevice};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Windows media controller backed by SMTC.
pub struct WindowsMediaControl {
    /// Raw Bluetooth address (e.g. `"AA:BB:CC:DD:EE:FF"`), if known.
    device_mac: String,
}

impl Default for WindowsMediaControl {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsMediaControl {
    /// Create a new controller. The device address should be set via
    /// [`MediaControl::set_device_address`] before use.
    pub fn new() -> Self {
        // WinRT async calls require the calling thread to be COM-initialised.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            device_mac: String::new(),
        }
    }

    /// Obtain the current media session from SMTC, if any.
    fn current_session(
        &self,
    ) -> Result<windows::Media::Control::GlobalSystemMediaTransportControlsSession, TransportError>
    {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .and_then(|op| op.join())
            .map_err(|e| TransportError::with_source("SMTC session manager unavailable", e))?;
        manager
            .GetCurrentSession()
            .map_err(|e| TransportError::with_source("no active SMTC media session", e))
    }

    /// Friendly name of the current default audio render endpoint.
    fn default_render_name(&self) -> Result<String, TransportError> {
        let id = MediaDevice::GetDefaultAudioRenderId(AudioDeviceRole::Default)
            .map_err(|e| TransportError::with_source("default render id lookup failed", e))?;
        let info = DeviceInformation::CreateFromIdAsync(&id)
            .and_then(|op| op.join())
            .map_err(|e| TransportError::with_source("default render lookup failed", e))?;
        info.Name()
            .map(|n| n.to_string())
            .map_err(|e| TransportError::with_source("render name lookup failed", e))
    }
}

impl MediaControl for WindowsMediaControl {
    fn set_device_address(&mut self, address: &str) {
        self.device_mac = address.to_string();
    }

    fn activate_audio(&self) -> Result<(), TransportError> {
        // Windows automatically routes A2DP audio to the AirPods on connect.
        log::debug!("Windows: A2DP routing is automatic; activate_audio is a no-op");
        Ok(())
    }

    fn remove_audio_output(&self) -> Result<(), TransportError> {
        log::debug!("Windows: audio output is removed by the OS on disconnect");
        Ok(())
    }

    fn play(&self) -> Result<(), TransportError> {
        let session = self.current_session()?;
        session
            .TryPlayAsync()
            .and_then(|op| op.join())
            .map_err(|e| TransportError::with_source("SMTC play failed", e))?;
        Ok(())
    }

    fn pause(&self) -> Result<(), TransportError> {
        let session = self.current_session()?;
        session
            .TryPauseAsync()
            .and_then(|op| op.join())
            .map_err(|e| TransportError::with_source("SMTC pause failed", e))?;
        Ok(())
    }

    fn set_volume(&self, _percent: u8) -> Result<(), TransportError> {
        Err(TransportError::new(
            "volume control is not implemented on Windows yet",
        ))
    }

    fn get_volume(&self) -> Result<u8, TransportError> {
        Err(TransportError::new(
            "volume control is not implemented on Windows yet",
        ))
    }

    fn is_airpods_active_sink(&self) -> bool {
        if self.device_mac.is_empty() {
            return false;
        }
        match self.default_render_name() {
            Ok(name) => name.to_ascii_lowercase().contains("airpods"),
            Err(e) => {
                log::debug!("could not determine default render device: {e}");
                false
            }
        }
    }
}
