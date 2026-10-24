// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

#![cfg(target_os = "linux")]

use librepods_daemon::{
    backend::{Backend, Endpoint, SessionTransport},
    config::Config,
    runtime,
    storage::Paths,
    DaemonError, ErrorCode,
};
use std::{sync::Arc, time::Duration};

struct CrashedOwner;
impl Backend for CrashedOwner {
    fn discover(&self, _: Option<&str>) -> Result<Endpoint, DaemonError> {
        panic!("injected owner failure")
    }
    fn connect(&self, _: &Endpoint, _: Duration) -> Result<Arc<dyn SessionTransport>, DaemonError> {
        unreachable!("discovery already failed")
    }
}

#[tokio::test]
async fn owner_failure_terminates_service_instead_of_publishing_stale_readiness() {
    let root = tempfile::tempdir().expect("temporary directory");
    let paths = Paths {
        runtime: root.path().join("run"),
        state: root.path().join("state"),
        config: root.path().join("config"),
        legacy_state: root.path().join("legacy/status.json"),
        legacy_socket: root.path().join("legacy/control.sock"),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        runtime::run(
            paths.clone(),
            Config::default(),
            Arc::new(CrashedOwner),
            std::future::pending(),
        ),
    )
    .await
    .expect("worker failure must wake supervisor");
    assert_eq!(
        result
            .expect_err("worker failure cannot be a successful service exit")
            .code,
        ErrorCode::ServiceUnavailable
    );
    assert!(!paths.socket().exists());
    assert!(!paths.state_file().exists());
}

#[test]
fn unavailable_audio_api_is_not_reported_as_a_bluetooth_connect_failure() {
    let error = librepods_linux::error::PlatformError::new(
        "audio API",
        librepods_linux::error::ErrorKind::Unavailable,
    );
    let mapped = librepods_daemon::backend::transport_error("audio", error.into());
    assert_eq!(mapped.code, ErrorCode::ServiceUnavailable);
    assert!(mapped.message.contains("audio API"));
}
