// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

#![cfg(target_os = "linux")]

use librepods_daemon::backend::{Backend, Endpoint, LinuxBackend};
use librepods_daemon::{ipc::Operation, ErrorCode};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

#[test]
fn backend_cancellation_is_latched_before_later_connects() {
    let backend = LinuxBackend::default();
    backend.cancel();
    let started = Instant::now();
    for _ in 0..3 {
        let result = backend.connect(
            &Endpoint {
                address: "AA:BB:CC:DD:EE:FF".into(),
                name: "test".into(),
            },
            Duration::from_secs(30),
        );
        assert!(matches!(result, Err(error) if error.code == ErrorCode::Cancelled));
    }
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[test]
fn unmute_targets_selected_airpods_instead_of_first_sink() {
    const CHILD: &str = "LIBREPODS_TEST_AUDIO_LOG";
    if let Some(log) = std::env::var_os(CHILD) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let backend = LinuxBackend::default();
        runtime
            .block_on(backend.audio_async("AA:BB:CC:DD:EE:FF".into(), Operation::Unmute))
            .expect("unmute");
        assert_eq!(
            std::fs::read_to_string(log).expect("action log"),
            "set-sink-mute bluez_output.AA_BB_CC_DD_EE_FF 0\n"
        );
        return;
    }
    // PATH is isolated in a child test process so parallel tests never inherit a fake audio server.
    let directory = tempfile::tempdir().expect("temporary fake pactl");
    let executable = directory.path().join("pactl");
    std::fs::write(&executable, r##"#!/usr/bin/python3
import json, os, sys
args = sys.argv[1:]
if args[0] == '--format=json':
    operation = args[1:]
    if operation == ['info']:
        print('{}')
    elif operation == ['list', 'sinks']:
        print(json.dumps([{'index': 1, 'name': 'speakers', 'mute': True}, {'index': 2, 'name': 'bluez_output.AA_BB_CC_DD_EE_FF', 'mute': True}]))
    else:
        print('[]')
else:
    with open(os.environ['LIBREPODS_TEST_AUDIO_LOG'], 'a') as log:
        log.write(' '.join(args) + '\n')
"##).expect("fake executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("executable permissions");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "unmute_targets_selected_airpods_instead_of_first_sink",
            "--nocapture",
        ])
        .env("PATH", directory.path())
        .env(CHILD, directory.path().join("actions.log"))
        .output()
        .expect("isolated child");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
