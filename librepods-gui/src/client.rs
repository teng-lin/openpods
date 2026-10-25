// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded, sequential IPC worker. It never opens Bluetooth, spawns a daemon, or replays a mutation.
use crate::model::Command;
use librepods_api::{
    ipc::{Operation, Request, PROTOCOL_VERSION},
    snapshot::Snapshot,
    transport, DaemonError, ErrorCode,
};
use serde_json::Value;
use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    time::{Duration, Instant},
};

pub enum Event {
    Status(Result<Box<(Snapshot, Value, Instant)>, String>),
    Finished(Result<(), String>),
}
pub struct Client {
    pub commands: SyncSender<Command>,
    pub events: Receiver<Event>,
}
impl Client {
    pub fn start(socket: std::path::PathBuf) -> std::io::Result<Self> {
        let (commands, rx) = mpsc::sync_channel::<Command>(1);
        let (tx, events) = mpsc::sync_channel(8);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("librepods-gui-ipc".into())
            .spawn(move || loop {
                let status = rt
                    .block_on(read_status(&socket))
                    .map(|(s, c)| Box::new((s, c, Instant::now())))
                    .map_err(describe);
                let _ = tx.try_send(Event::Status(status));
                match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(command) => {
                        let result = rt.block_on(execute(&socket, &command)).map_err(describe);
                        if tx.send(Event::Finished(result)).is_err() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            })?;
        Ok(Self { commands, events })
    }
}
fn describe(e: DaemonError) -> String {
    if e.code == ErrorCode::OutcomeUnknown {
        format!("Outcome unknown. The request may have applied. Check current state before trying again. {}",e.message)
    } else {
        format!("{:?}: {}", e.code, e.message)
    }
}
pub async fn exchange(
    socket: &std::path::Path,
    op: Operation,
    id: u64,
    timeout_ms: u64,
) -> Result<Value, DaemonError> {
    let response = transport::request(
        socket,
        &Request {
            version: PROTOCOL_VERSION,
            id,
            timeout_ms,
            operation: op,
        },
    )
    .await?;
    if let Some(e) = response.error {
        return Err(e);
    }
    response
        .result
        .ok_or_else(|| DaemonError::new(ErrorCode::MalformedFrame, "gui", "No response result"))
}
pub async fn read_status(socket: &std::path::Path) -> Result<(Snapshot, Value), DaemonError> {
    let value = exchange(socket, Operation::Status, 1, 1000).await?;
    let s: Snapshot = serde_json::from_value(value)
        .map_err(|e| DaemonError::new(ErrorCode::MalformedFrame, "gui status", e.to_string()))?;
    if s.schema_version != 1 {
        return Err(DaemonError::new(
            ErrorCode::VersionMismatch,
            "gui status",
            "Unsupported status schema",
        ));
    }
    // Older daemons omit new flags. Missing flags always disable those actions.
    let caps = exchange(socket, Operation::Capabilities, 2, 1000).await?;
    Ok((s, caps))
}
pub async fn execute(socket: &std::path::Path, command: &Command) -> Result<(), DaemonError> {
    let (s, caps) = read_status(socket).await?;
    command
        .validate(&s, &caps)
        .map_err(|e| DaemonError::new(ErrorCode::Cancelled, "gui preflight", e))?;
    exchange(socket, command.operation(), 3, 6000).await?;
    Ok(())
}

/// Resolve only the current user's native socket; no service directories are created.
pub fn socket_from_env() -> Result<std::path::PathBuf, DaemonError> {
    let path = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        DaemonError::new(
            ErrorCode::InvalidInput,
            "gui runtime",
            "XDG_RUNTIME_DIR is required",
        )
    })?;
    socket_in(std::path::Path::new(&path))
}
pub fn socket_in(runtime: &std::path::Path) -> Result<std::path::PathBuf, DaemonError> {
    if !runtime.is_absolute() {
        return Err(DaemonError::new(
            ErrorCode::InvalidInput,
            "gui runtime",
            "XDG_RUNTIME_DIR must be absolute",
        ));
    }
    Ok(runtime.join("librepods-rs/control.sock"))
}
