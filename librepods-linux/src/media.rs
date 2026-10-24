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
    #[test]
    fn compatibility_routes_and_volume_use_only_the_selected_existing_device() {
        const CHILD: &str = "LIBREPODS_MEDIA_TEST_LOG";
        if let Some(log) = std::env::var_os(CHILD) {
            let mut media = LinuxMediaControl::new();
            MediaControl::set_device_address(&mut media, "AA:BB:CC:DD:EE:FF");
            assert_eq!(media.get_volume().expect("selected volume"), 50);
            assert!(media.is_airpods_active_sink());
            MediaControl::activate_audio(&media).expect("explicit route");
            media.set_volume(25).expect("selected volume mutation");
            assert_eq!(
                crate::error::kind(&media.set_volume(101).expect_err("invalid volume")),
                Some(ErrorKind::InvalidInput)
            );
            MediaControl::remove_audio_output(&media).expect("no profile cleanup");
            media.set_device_address("11:22:33:44:55:66");
            assert!(!media.is_airpods_active_sink());
            assert_eq!(
                crate::error::kind(&media.get_volume().expect_err("missing volume")),
                Some(ErrorKind::Unavailable)
            );
            assert_eq!(
                crate::error::kind(&media.activate_audio().expect_err("missing output")),
                Some(ErrorKind::Unavailable)
            );
            assert_eq!(
                crate::error::kind(&media.set_volume(20).expect_err("missing output")),
                Some(ErrorKind::Unavailable)
            );
            assert_eq!(std::fs::read_to_string(log).expect("mutations"), "set-default-sink bluez_output.AA_BB_CC_DD_EE_FF\nset-sink-volume bluez_output.AA_BB_CC_DD_EE_FF 25%\n");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("fake audio directory");
        let executable = directory.path().join("pactl");
        std::fs::write(&executable, r##"#!/usr/bin/python3
import json, os, sys
args = sys.argv[1:]
if args[0] == '--format=json':
    operation = args[1:]
    if operation == ['info']:
        print(json.dumps({'server_name':'test','default_sink_name':'bluez_output.AA_BB_CC_DD_EE_FF','default_source_name':'external_microphone'}))
    elif operation == ['list', 'sinks']:
        print(json.dumps([{'index':1,'name':'speakers'}, {'index':2,'name':'bluez_output.AA_BB_CC_DD_EE_FF','volume':{'left':{'value':32768},'right':{'value':32768}}}]))
    elif operation == ['list', 'cards']:
        print(json.dumps([{'name':'bluez_card.AA_BB_CC_DD_EE_FF','active_profile':'a2dp-sink','profiles':{'a2dp-sink':{'available':True,'sinks':1,'sources':0}}}]))
    else:
        print('[]')
else:
    with open(os.environ['LIBREPODS_MEDIA_TEST_LOG'], 'a') as log:
        log.write(' '.join(args) + '\n')
"##).expect("fake executable");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "media::tests::compatibility_routes_and_volume_use_only_the_selected_existing_device", "--nocapture"])
            .env("PATH", directory.path()).env(CHILD, directory.path().join("mutations.log"))
            .output().expect("child process");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
