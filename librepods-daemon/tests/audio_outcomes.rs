// SPDX-License-Identifier: AGPL-3.0-only
#![cfg(target_os = "linux")]
use librepods_daemon::{
    backend::{Backend, Endpoint, SessionTransport},
    config::Config,
    ipc::{Operation, Request},
    session::Session,
    DaemonError, ErrorCode,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::Arc,
    time::{Duration, Instant},
};

struct TimeoutBackend;
impl Backend for TimeoutBackend {
    fn discover(&self, _: Option<&str>) -> Result<Endpoint, DaemonError> {
        Err(DaemonError::transient(
            ErrorCode::AdapterUnavailable,
            "fake",
            "adapter absent",
        ))
    }
    fn connect(&self, _: &Endpoint, _: Duration) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        Err(DaemonError::new(
            ErrorCode::ConnectFailed,
            "fake",
            "no endpoint",
        ))
    }
    fn audio(&self, _: &str, _: &Operation) -> Result<serde_json::Value, DaemonError> {
        Err(DaemonError::new(
            ErrorCode::CommandTimeout,
            "audio action",
            "injected backend timeout",
        ))
    }
}

#[tokio::test]
async fn backend_timeout_is_uncertain_for_audio_mutation_but_precise_for_status() {
    let session = Session::spawn(
        Arc::new(TimeoutBackend),
        Config::default(),
        "audio-errors".into(),
    );
    for (operation, expected) in [
        (Operation::AudioStatus, ErrorCode::CommandTimeout),
        (
            Operation::SetInput("example-input".into()),
            ErrorCode::OutcomeUnknown,
        ),
    ] {
        let response = session
            .handle
            .request(Request {
                version: 1,
                id: 1,
                timeout_ms: 1000,
                operation,
            })
            .await;
        assert_eq!(response.error.expect("backend timeout").code, expected);
    }
    session.stop();
}

#[test]
fn native_audio_outcome_child_probe() {
    let Ok(mode) = std::env::var("LIBREPODS_AUDIO_FAILURE_MODE") else {
        return;
    };
    tokio::runtime::Runtime::new()
        .expect("child runtime")
        .block_on(async {
            let backend = librepods_daemon::backend::LinuxBackend::default();
            let error = backend
                .audio_async(
                    "00:11:22:33:44:55".into(),
                    Operation::SetInput("example-input".into()),
                )
                .await
                .expect_err("audio failure injected after dispatch");
            assert_eq!(error.code, ErrorCode::OutcomeUnknown, "{mode}: {error:?}");
            let marker = std::env::var_os("LIBREPODS_AUDIO_MARKER").expect("action marker");
            assert_eq!(
                serde_json::from_slice::<Vec<String>>(&fs::read(marker).expect("action executed"))
                    .expect("arguments"),
                vec!["set-default-source", "example-input"]
            );
            if mode == "readback-fail" {
                assert!(
                    error.stage.contains("readback") || error.message.contains("readback"),
                    "readback cause was lost: {error:?}"
                );
            }
        });
}

#[test]
fn real_audio_adapter_preserves_uncertainty_after_action_timeout_or_failed_readback() {
    for mode in ["readback-fail", "action-timeout"] {
        let directory = tempfile::tempdir().expect("private directory");
        let executable = directory.path().join("pactl");
        fs::write(&executable, r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
args = sys.argv[1:]
if args[:2] == ['-f', 'json']: args = args[2:]
if args[:1] == ['--format=json']: args = args[1:]
marker = pathlib.Path(os.environ['LIBREPODS_AUDIO_MARKER'])
mode = os.environ['LIBREPODS_AUDIO_FAILURE_MODE']
if args[0].startswith('set-'):
    marker.write_text(json.dumps(args))
    if mode == 'action-timeout': time.sleep(10)
    sys.exit(0)
if marker.exists() and mode == 'readback-fail': sys.exit(91)
if args == ['info']: print(json.dumps({'server_name':'synthetic PipeWire','default_source_name':'example-input'}))
elif args == ['list','sources']: print(json.dumps([{'index':7,'name':'example-input','mute':False,'properties':{}}]))
else: print('[]')
"#).expect("fake audio API client");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "native_audio_outcome_child_probe", "--nocapture"])
                .env("PATH", directory.path())
                .env(
                    "DBUS_SYSTEM_BUS_ADDRESS",
                    format!("unix:path={}/no-system-bus", directory.path().display()),
                )
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}/no-session-bus", directory.path().display()),
                )
                .env(
                    "PULSE_SERVER",
                    format!("unix:{}/no-pulse", directory.path().display()),
                )
                .env("LIBREPODS_AUDIO_FAILURE_MODE", mode)
                .env(
                    "LIBREPODS_AUDIO_MARKER",
                    directory.path().join("action.json"),
                )
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("isolated audio outcome probe");
        let deadline = Instant::now() + Duration::from_secs(6);
        while child.try_wait().expect("child status").is_none() {
            if Instant::now() >= deadline {
                child.kill().expect("kill stalled probe");
                child.wait().expect("reap probe");
                panic!("{mode} audio probe exceeded bound");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().expect("child output");
        assert!(
            output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
