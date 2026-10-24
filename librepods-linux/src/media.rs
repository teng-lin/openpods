// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Compatibility facade over bounded, structured desktop adapters.
//!
//! New services should use `desktop` directly and keep automation ownership in
//! their session policy. This facade never disables profiles during cleanup.

use std::future::Future;
use std::time::Duration;

use librepods_core::transport::{MediaControl, TransportError};

use crate::desktop::{self, AudioAction};
use crate::error::{ErrorKind, PlatformError};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(3);

fn run<T: Send + 'static>(
    future: impl Future<Output = Result<T, TransportError>> + Send + 'static,
) -> Result<T, TransportError> {
    // A dedicated thread also permits compatibility calls from inside a Tokio runtime.
    std::thread::Builder::new()
        .name("librepods-media-compat".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    PlatformError::io("create desktop compatibility runtime", error)
                })?;
            runtime.block_on(async {
                tokio::time::timeout(OPERATION_TIMEOUT, future)
                    .await
                    .map_err(|_| {
                        PlatformError::new("desktop compatibility operation", ErrorKind::Timeout)
                    })?
            })
        })
        .map_err(|error| PlatformError::io("start desktop compatibility worker", error))?
        .join()
        .map_err(|_| PlatformError::new("desktop compatibility worker failed", ErrorKind::Io))?
}

#[derive(Default)]
pub struct LinuxMediaControl {
    address: String,
}

impl LinuxMediaControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_device_address(&mut self, address: &str) {
        self.address = address.to_owned();
    }

    /// Explicitly select the existing output. Profile creation is a separate action.
    pub fn activate_audio(&self) -> Result<(), TransportError> {
        let address = self.address.clone();
        run(async move {
            let snapshot = desktop::audio_snapshot(&address).await?;
            let output = snapshot
                .outputs
                .into_iter()
                .find(|node| node.selected_device)
                .ok_or_else(|| {
                    PlatformError::new(
                        "selected AirPods output unavailable",
                        ErrorKind::Unavailable,
                    )
                })?;
            desktop::audio_action(
                &address,
                AudioAction::SetDefaultOutput { name: output.name },
            )
            .await
        })
    }

    /// Device/node removal belongs to the audio server, not this library facade.
    pub fn remove_audio_output(&self) -> Result<(), TransportError> {
        Ok(())
    }
}

impl MediaControl for LinuxMediaControl {
    fn set_device_address(&mut self, address: &str) {
        Self::set_device_address(self, address);
    }
    fn activate_audio(&self) -> Result<(), TransportError> {
        Self::activate_audio(self)
    }
    fn remove_audio_output(&self) -> Result<(), TransportError> {
        Self::remove_audio_output(self)
    }

    fn play(&self) -> Result<(), TransportError> {
        Err(PlatformError::new(
            "global play is unsupported; select a confirmed MPRIS player",
            ErrorKind::Unsupported,
        )
        .into())
    }

    fn pause(&self) -> Result<(), TransportError> {
        Err(PlatformError::new(
            "global pause is unsupported; select a confirmed MPRIS player",
            ErrorKind::Unsupported,
        )
        .into())
    }

    fn set_volume(&self, percent: u8) -> Result<(), TransportError> {
        let address = self.address.clone();
        run(async move {
            let snapshot = desktop::audio_snapshot(&address).await?;
            let output = snapshot
                .outputs
                .into_iter()
                .find(|node| node.selected_device)
                .ok_or_else(|| {
                    PlatformError::new(
                        "selected AirPods output unavailable",
                        ErrorKind::Unavailable,
                    )
                })?;
            desktop::audio_action(
                &address,
                AudioAction::SetVolume {
                    name: output.name,
                    input: false,
                    percent,
                },
            )
            .await
        })
    }

    fn get_volume(&self) -> Result<u8, TransportError> {
        let address = self.address.clone();
        run(async move {
            let snapshot = desktop::audio_snapshot(&address).await?;
            snapshot
                .outputs
                .into_iter()
                .find(|node| node.selected_device)
                .and_then(|node| node.volume_percent)
                .map(|volume| volume.round().clamp(0.0, u8::MAX as f64) as u8)
                .ok_or_else(|| {
                    PlatformError::new(
                        "selected AirPods volume unavailable",
                        ErrorKind::Unavailable,
                    )
                    .into()
                })
        })
    }

    fn is_airpods_active_sink(&self) -> bool {
        let address = self.address.clone();
        run(async move {
            let snapshot = desktop::audio_snapshot(&address).await?;
            Ok(snapshot.outputs.iter().any(|output| {
                output.selected_device && Some(&output.name) == snapshot.default_output.as_ref()
            }))
        })
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_facade_cannot_resume_or_pause_an_unowned_player() {
        let media = LinuxMediaControl::new();
        assert_eq!(
            crate::error::kind(&media.play().expect_err("no ownership")),
            Some(ErrorKind::Unsupported)
        );
        assert_eq!(
            crate::error::kind(&media.pause().expect_err("no ownership")),
            Some(ErrorKind::Unsupported)
        );
        media
            .remove_audio_output()
            .expect("cleanup makes no mutation");
    }

    #[tokio::test]
    async fn compatibility_calls_can_run_inside_a_runtime() {
        assert_eq!(run(async { Ok(42) }).expect("separate runtime"), 42);
    }
}
