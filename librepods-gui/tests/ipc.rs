// SPDX-License-Identifier: AGPL-3.0-only
#![cfg(target_os = "linux")]
use librepods_api::{
    ipc::{Operation, Request, Response},
    transport as runtime, ErrorCode,
};
use librepods_gui::{
    client::{self, Client, Event},
    demo,
    model::{Action, Command, Target},
};
use serde_json::json;
use std::time::Duration;
use tokio::{io::BufReader, net::UnixListener};
fn paths() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    (dir, socket)
}
async fn reply(listener: &UnixListener, f: impl FnOnce(Request) -> Response) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let bytes = runtime::read_frame(&mut BufReader::new(&mut stream))
        .await
        .unwrap()
        .unwrap();
    let req: Request = serde_json::from_slice(&bytes).unwrap();
    runtime::write_response(&mut stream, &f(req)).await.unwrap();
}
#[tokio::test]
async fn reads_native_state_and_detects_missing_old_daemon_capabilities() {
    let (_dir, p) = paths();
    let listener = UnixListener::bind(&p).unwrap();
    let server = tokio::spawn(async move {
        reply(&listener, |r| {
            assert!(matches!(r.operation, Operation::Status));
            Response::ok(r.id, demo::snapshot())
        })
        .await;
        reply(&listener, |r| {
            assert!(matches!(r.operation, Operation::Capabilities));
            Response::ok(r.id, json!({}))
        })
        .await;
    });
    let (s, c) = client::read_status(&p).await.unwrap();
    assert!(s.connected);
    assert!(c.get("device_management").is_none());
    server.await.unwrap();
}
#[tokio::test]
async fn command_preflight_cancels_session_change_before_mutating() {
    let (_dir, p) = paths();
    let listener = UnixListener::bind(&p).unwrap();
    let original = demo::snapshot();
    let cmd = Command {
        target: Target::from_snapshot(&original),
        action: Action::Disconnect,
    };
    let server = tokio::spawn(async move {
        let mut s = demo::snapshot();
        s.session_generation += 1;
        let caps = s.capabilities();
        reply(&listener, |r| Response::ok(r.id, s)).await;
        reply(&listener, |r| Response::ok(r.id, caps)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
    assert_eq!(
        client::execute(&p, &cmd).await.unwrap_err().code,
        ErrorCode::Cancelled
    );
    server.await.unwrap();
}
#[tokio::test]
async fn command_is_targeted_and_never_replayed_after_lost_reply() {
    let (_dir, p) = paths();
    let listener = UnixListener::bind(&p).unwrap();
    let s = demo::snapshot();
    let cmd = Command {
        target: Target::from_snapshot(&s),
        action: Action::Disconnect,
    };
    let server = tokio::spawn(async move {
        let s = demo::snapshot();
        let caps = s.capabilities();
        reply(&listener, |r| Response::ok(r.id, s)).await;
        reply(&listener, |r| Response::ok(r.id, caps)).await;
        let (mut stream, _) = listener.accept().await.unwrap();
        let b = runtime::read_frame(&mut BufReader::new(&mut stream))
            .await
            .unwrap()
            .unwrap();
        let r: Request = serde_json::from_slice(&b).unwrap();
        assert!(
            matches!(r.operation,Operation::ForDevice{address,operation} if address=="00:11:22:33:44:55" && matches!(*operation,Operation::DeviceDisconnect))
        );
        drop(stream);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
    assert_eq!(
        client::execute(&p, &cmd).await.unwrap_err().code,
        ErrorCode::OutcomeUnknown
    );
    server.await.unwrap();
}
#[tokio::test]
async fn validated_error_and_corrupt_response_propagate() {
    let (_dir, p) = paths();
    let listener = UnixListener::bind(&p).unwrap();
    let server = tokio::spawn(async move {
        reply(&listener, |r| {
            Response::failed(
                r.id,
                librepods_api::DaemonError::new(ErrorCode::PermissionDenied, "test", "denied"),
            )
        })
        .await;
        reply(&listener, |r| Response::ok(r.id + 1, json!({}))).await;
        reply(&listener, |r| {
            let mut s = demo::snapshot();
            s.schema_version = 9;
            Response::ok(r.id, s)
        })
        .await;
        reply(&listener, |r| Response::ok(r.id, json!({"bad":"schema"}))).await;
    });
    assert_eq!(
        client::exchange(&p, Operation::Status, 1, 1000)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        client::exchange(&p, Operation::Status, 1, 1000)
            .await
            .unwrap_err()
            .code,
        ErrorCode::VersionMismatch
    );
    assert_eq!(
        client::read_status(&p).await.unwrap_err().code,
        ErrorCode::VersionMismatch
    );
    assert_eq!(
        client::read_status(&p).await.unwrap_err().code,
        ErrorCode::MalformedFrame
    );
    server.await.unwrap();
}
#[tokio::test]
async fn successful_mutation_requires_daemon_reply() {
    let (_dir, p) = paths();
    let listener = UnixListener::bind(&p).unwrap();
    let s = demo::snapshot();
    let cmd = Command {
        target: Target::from_snapshot(&s),
        action: Action::Conversation(true),
    };
    let server = tokio::spawn(async move {
        let s = demo::snapshot();
        let caps = s.capabilities();
        reply(&listener, |r| Response::ok(r.id, s)).await;
        reply(&listener, |r| Response::ok(r.id, caps)).await;
        reply(&listener, |r| Response::ok(r.id, json!({"confirmed":true}))).await;
    });
    client::execute(&p, &cmd).await.unwrap();
    server.await.unwrap();
}
#[test]
fn worker_recovers_after_daemon_appears_and_handles_confirmed_commands() {
    let (_dir, p) = paths();
    let client = Client::start(p.clone()).unwrap();
    assert!(matches!(
        client.events.recv_timeout(Duration::from_secs(2)).unwrap(),
        Event::Status(Err(_))
    ));
    let thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = UnixListener::bind(&p).unwrap();
                for _ in 0..2 {
                    let s = demo::snapshot();
                    let caps = s.capabilities();
                    reply(&listener, |r| Response::ok(r.id, s)).await;
                    reply(&listener, |r| Response::ok(r.id, caps)).await;
                }
                reply(&listener, |r| Response::ok(r.id, json!({"ok":true}))).await;
                // Read-only refresh after completion.
                let s = demo::snapshot();
                let caps = s.capabilities();
                reply(&listener, |r| Response::ok(r.id, s)).await;
                reply(&listener, |r| Response::ok(r.id, caps)).await;
            })
    });
    let status = client.events.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(status, Event::Status(Ok(_))));
    let s = demo::snapshot();
    client
        .commands
        .send(Command {
            target: Target::from_snapshot(&s),
            action: Action::Disconnect,
        })
        .unwrap();
    assert!(matches!(
        client.events.recv_timeout(Duration::from_secs(3)).unwrap(),
        Event::Finished(Ok(()))
    ));
    assert!(matches!(
        client.events.recv_timeout(Duration::from_secs(3)).unwrap(),
        Event::Status(Ok(_))
    ));
    drop(client);
    thread.join().unwrap();
}

#[test]
fn socket_path_is_native_absolute_and_never_creates_service_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = client::socket_in(dir.path()).unwrap();
    assert_eq!(path, dir.path().join("librepods-rs/control.sock"));
    assert!(!path.parent().unwrap().exists());
    assert_eq!(
        client::socket_in(std::path::Path::new("relative"))
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
}

#[test]
fn environment_socket_resolution_reports_the_actual_environment() {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(path) => assert_eq!(
            client::socket_from_env(),
            client::socket_in(std::path::Path::new(&path))
        ),
        None => assert_eq!(
            client::socket_from_env().unwrap_err().code,
            ErrorCode::InvalidInput
        ),
    }
}
#[test]
fn worker_reports_unknown_outcome_without_retrying_the_mutation() {
    let (_dir, p) = paths();
    let socket = p.clone();
    let (ready, started) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = UnixListener::bind(socket).unwrap();
                ready.send(()).unwrap();
                for _ in 0..2 {
                    let s = demo::snapshot();
                    let caps = s.capabilities();
                    reply(&listener, |r| Response::ok(r.id, s)).await;
                    reply(&listener, |r| Response::ok(r.id, caps)).await;
                }
                let (mut stream, _) = listener.accept().await.unwrap();
                assert!(runtime::read_frame(&mut BufReader::new(&mut stream))
                    .await
                    .unwrap()
                    .is_some());
                drop(stream);
            })
    });
    started.recv().unwrap();
    let client = Client::start(p).unwrap();
    assert!(matches!(
        client.events.recv_timeout(Duration::from_secs(2)).unwrap(),
        Event::Status(Ok(_))
    ));
    let s = demo::snapshot();
    client
        .commands
        .send(Command {
            target: Target::from_snapshot(&s),
            action: Action::Disconnect,
        })
        .unwrap();
    match client.events.recv_timeout(Duration::from_secs(2)).unwrap() {
        Event::Finished(Err(e)) => assert!(e.contains("Outcome unknown")),
        _ => panic!("expected explicit uncertainty"),
    }
    drop(client);
    server.join().unwrap();
}
