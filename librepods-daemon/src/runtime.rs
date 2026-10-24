// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

use crate::{
    backend::Backend,
    config::Config,
    ipc::{self, Operation, Request, Response},
    session::{Session, SessionHandle},
    snapshot::Snapshot,
    storage::{
        atomic_write, publish_legacy, read_private_json, restrict_socket, OwnerGuard, Paths,
    },
    DaemonError, ErrorCode,
};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

pub fn instance_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub async fn serve(
    paths: Paths,
    config: Config,
    backend: Arc<dyn Backend>,
) -> Result<(), DaemonError> {
    run(paths, config, backend, async {
        let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        match term {
            Ok(mut term) => {
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    })
    .await
}

pub async fn run(
    paths: Paths,
    config: Config,
    backend: Arc<dyn Backend>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), DaemonError> {
    config.validate()?;
    let instance = instance_id();
    let mut owner = OwnerGuard::acquire(&paths, &instance)?;
    if paths.legacy_socket.exists() {
        return Err(DaemonError::new(
            ErrorCode::ServiceUnavailable,
            "ownership",
            "legacy daemon socket exists; stop the previous connection owner before starting Rust",
        ));
    }
    let listener = UnixListener::bind(paths.socket()).map_err(|e| DaemonError::io("listen", e))?;
    restrict_socket(&paths.socket())?;
    owner.record_socket()?;
    let desktop_enabled = backend.desktop_enabled();
    let session =
        Session::spawn_persistent(backend, config.clone(), instance.clone(), paths.clone());
    let (desktop_stop, desktop_shutdown) = tokio::sync::watch::channel(false);
    let desktop = if desktop_enabled {
        Some(tokio::spawn(crate::desktop::run(
            config.clone(),
            session.handle.status.clone(),
            session.handle.audio_feed.clone(),
            session.handle.manual_overrides.subscribe(),
            desktop_shutdown,
        )))
    } else {
        None
    };
    let mut publication = session.handle.status.clone();
    let publisher_paths = paths.clone();
    let publication_health = session.handle.publication_health.clone();
    let legacy = config.legacy_compatibility;
    let (publisher_stop, mut publisher_shutdown) = tokio::sync::watch::channel(false);
    let publisher = tokio::spawn(async move {
        let mut previous = Vec::new();
        loop {
            if *publisher_shutdown.borrow() {
                break;
            }
            let snapshot = publication.borrow_and_update().clone();
            let bytes = match serde_json::to_vec(&snapshot) {
                Ok(bytes) => bytes,
                Err(error) => {
                    log::error!("status encoding failed: {error}");
                    break;
                }
            };
            if bytes != previous {
                let path = publisher_paths.state_file();
                let clone = bytes.clone();
                let result = tokio::task::spawn_blocking(move || atomic_write(&path, &clone)).await;
                let mut error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(DaemonError::new(
                        ErrorCode::PersistenceFailed,
                        "status publication",
                        error.to_string(),
                    )),
                    Err(error) => Some(DaemonError::new(
                        ErrorCode::PersistenceFailed,
                        "status publisher",
                        error.to_string(),
                    )),
                };
                if legacy {
                    let paths = publisher_paths.clone();
                    let instance = snapshot.daemon_instance.clone();
                    let legacy_bytes = serde_json::to_vec(&snapshot.legacy())
                        .expect("legacy DTO contains only JSON values");
                    let legacy_result = tokio::task::spawn_blocking(move || {
                        publish_legacy(&paths, &instance, &legacy_bytes)
                    })
                    .await;
                    let legacy_error = match legacy_result {
                        Ok(result) => result.err(),
                        Err(join) => Some(DaemonError::new(
                            ErrorCode::PersistenceFailed,
                            "legacy publisher",
                            join.to_string(),
                        )),
                    };
                    error = error.or(legacy_error);
                }
                if error.is_none() {
                    previous = bytes;
                }
                publication_health.send_if_modified(|current| {
                    if *current == error {
                        false
                    } else {
                        *current = error;
                        true
                    }
                });
            }
            if publication_health.borrow().is_some() {
                tokio::select! {changed=publication.changed()=>{if changed.is_err(){break;}},_=publisher_shutdown.changed()=>break,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
            } else {
                tokio::select! {changed=publication.changed()=>{if changed.is_err(){break;}},_=publisher_shutdown.changed()=>break}
            }
        }
    });
    let permits = Arc::new(Semaphore::new(32));
    let mut clients = tokio::task::JoinSet::new();
    let mut owner_health = session.handle.status.clone();
    tokio::pin!(shutdown);
    let outcome = loop {
        tokio::select! {
            _=&mut shutdown=>break Ok(()),
            changed=owner_health.changed()=>{
                if changed.is_err() {
                    break Err(DaemonError::new(ErrorCode::ServiceUnavailable,"session owner","session worker exited unexpectedly"));
                }
            },
            Some(_)=clients.join_next(),if !clients.is_empty()=>{},
            result=listener.accept()=>{
                let (stream,_) = match result { Ok(accepted)=>accepted, Err(error)=>break Err(DaemonError::io("accept", error)) };
                let Ok(credentials)=stream.peer_cred() else {continue;};
                // Safety: getuid has no preconditions.
                if credentials.uid()!=unsafe{libc::getuid()}{continue;}
                let Ok(permit)=permits.clone().try_acquire_owned() else {continue;};
                let handle=session.handle.clone();
                let paths=paths.clone();
                let config=config.clone();
                clients.spawn(async move {let _permit=permit;let _=client_connection(stream,handle,paths,config).await;});
            },
        }
    };
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    desktop_stop.send_replace(true);
    session.handle.stop();
    let session_stop = tokio::task::spawn_blocking(move || session.stop());
    if let Some(mut desktop) = desktop {
        if tokio::time::timeout(Duration::from_secs(1), &mut desktop)
            .await
            .is_err()
        {
            desktop.abort();
            let _ = desktop.await;
        }
    }
    // Both owners begin cancellation together; desktop cleanup does not delay AACP shutdown.
    session_stop
        .await
        .map_err(|e| DaemonError::new(ErrorCode::Internal, "shutdown", e.to_string()))?;
    // Wait for any atomic write already in flight before removing owned artifacts.
    publisher_stop.send_replace(true);
    let _ = publisher.await;
    if legacy {
        if let Ok(Some(value)) = read_private_json(&paths.legacy_state) {
            if value["daemon_instance"].as_str() == Some(instance.as_str()) {
                let _ = std::fs::remove_file(&paths.legacy_state);
            }
        }
    }
    outcome
}

async fn client_connection(
    stream: UnixStream,
    handle: SessionHandle,
    _paths: Paths,
    _config: Config,
) -> Result<(), DaemonError> {
    let (mut reader, mut writer) = stream.into_split();
    let mut buffered = Vec::new();
    loop {
        let bytes = match tokio::time::timeout(
            Duration::from_secs(5),
            read_buffered_frame(&mut reader, &mut buffered),
        )
        .await
        {
            Ok(Ok(Some(bytes))) => bytes,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(error)) => {
                write_response(&mut writer, &Response::failed(0, error)).await?;
                return Ok(());
            }
            Err(_) => {
                return Err(DaemonError::new(
                    ErrorCode::CommandTimeout,
                    "ipc read",
                    "client stopped sending",
                ))
            }
        };
        let request = match ipc::decode_request(&bytes) {
            Ok(request) => request,
            Err(error) => {
                write_response(&mut writer, &Response::failed(0, error)).await?;
                continue;
            }
        };
        let watch_target = match &request.operation {
            Operation::Watch => Some(None),
            Operation::ForDevice { address, operation }
                if matches!(operation.as_ref(), Operation::Watch) =>
            {
                Some(Some(address.clone()))
            }
            _ => None,
        };
        if let Some(target) = watch_target {
            let id = request.id;
            let mut snapshots = handle.status.clone();
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                if target
                    .as_ref()
                    .is_some_and(|address| !address.eq_ignore_ascii_case(&snapshot.device.address))
                {
                    write_response(
                        &mut writer,
                        &Response::failed(
                            id,
                            DaemonError::new(
                                ErrorCode::DeviceUnavailable,
                                "device selection",
                                "service owns a different device",
                            ),
                        ),
                    )
                    .await?;
                    return Ok(());
                }
                write_response(&mut writer, &Response::snapshot(id, &snapshot)).await?;
                let mut probe = [0u8; 1];
                tokio::select! {
                    changed=snapshots.changed()=>{if changed.is_err(){return Ok(());}},
                    _=reader.read(&mut probe)=>return Ok(()),
                }
            }
        }
        let pending = handle.request(request);
        tokio::pin!(pending);
        let response = loop {
            let mut input = [0u8; 1024];
            tokio::select! {
                biased;
                read=reader.read(&mut input)=>{
                    let size=read.map_err(|e|DaemonError::io("IPC pending read",e))?;
                    if size==0{return Ok(());}
                    if buffered.len()+size>ipc::MAX_FRAME*2{return Err(DaemonError::new(ErrorCode::QueueFull,"IPC","too many pipelined bytes"));}
                    buffered.extend_from_slice(&input[..size]);
                },
                response=&mut pending=>break response,
            }
        };
        write_response(&mut writer, &response).await?;
    }
}

async fn read_buffered_frame(
    reader: &mut (impl AsyncRead + Unpin),
    buffered: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>, DaemonError> {
    loop {
        if let Some(position) = buffered.iter().position(|byte| *byte == b'\n') {
            if position > ipc::MAX_FRAME {
                return Err(DaemonError::new(
                    ErrorCode::InvalidInput,
                    "IPC",
                    "frame exceeds16KiB",
                ));
            }
            let frame = buffered.drain(..=position).collect::<Vec<_>>();
            return Ok(Some(frame[..position].to_vec()));
        }
        if buffered.len() > ipc::MAX_FRAME {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "IPC",
                "frame exceeds16KiB",
            ));
        }
        let mut input = [0u8; 1024];
        let size = reader
            .read(&mut input)
            .await
            .map_err(|e| DaemonError::io("IPC read", e))?;
        if size == 0 {
            return if buffered.is_empty() {
                Ok(None)
            } else {
                Err(DaemonError::new(
                    ErrorCode::MalformedFrame,
                    "IPC",
                    "EOF within frame",
                ))
            };
        }
        buffered.extend_from_slice(&input[..size]);
    }
}

pub async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<Vec<u8>>, DaemonError> {
    read_frame_limit(reader, ipc::MAX_FRAME).await
}

pub async fn read_response_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<Vec<u8>>, DaemonError> {
    read_frame_limit(reader, ipc::MAX_RESPONSE_FRAME).await
}

async fn read_frame_limit(
    reader: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> Result<Option<Vec<u8>>, DaemonError> {
    let mut frame = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let size = reader
            .read(&mut byte)
            .await
            .map_err(|e| DaemonError::io("IPC read", e))?;
        if size == 0 {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(DaemonError::new(
                    ErrorCode::MalformedFrame,
                    "ipc",
                    "connection closed within a frame",
                ))
            };
        }
        if byte[0] == b'\n' {
            return Ok(Some(frame));
        }
        if frame.len() == limit {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "ipc",
                format!("frame exceeds {limit} byte limit"),
            ));
        }
        frame.push(byte[0]);
    }
}

pub async fn write_response(
    writer: &mut (impl AsyncWrite + Unpin),
    response: &Response,
) -> Result<(), DaemonError> {
    let mut bytes = serde_json::to_vec(response)
        .map_err(|e| DaemonError::new(ErrorCode::Internal, "ipc encode", e.to_string()))?;
    if bytes.len() > ipc::MAX_RESPONSE_FRAME {
        bytes = serde_json::to_vec(&Response::failed(
            response.id,
            DaemonError::new(
                ErrorCode::ResponseTooLarge,
                "ipc encode",
                "response exceeds 128 KiB; inspect status before repeating a mutation",
            ),
        ))
        .map_err(|error| DaemonError::new(ErrorCode::Internal, "ipc encode", error.to_string()))?;
    }
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(1), writer.write_all(&bytes))
        .await
        .map_err(|_| {
            DaemonError::new(
                ErrorCode::CommandTimeout,
                "ipc write",
                "client stopped reading",
            )
        })?
        .map_err(|e| DaemonError::io("ipc write", e))
}

pub async fn request(paths: &Paths, request: &Request) -> Result<Response, DaemonError> {
    let duration = Duration::from_millis(request.timeout_ms);
    let mutation = request.operation.is_mutation();
    let mut dispatched = false;
    tokio::time::timeout(duration, async {
        let mut stream = UnixStream::connect(paths.socket())
            .await
            .map_err(|e| DaemonError::io("connect daemon", e))?;
        let mut bytes = serde_json::to_vec(request)
            .map_err(|e| DaemonError::new(ErrorCode::InvalidInput, "request", e.to_string()))?;
        if bytes.len() > ipc::MAX_FRAME {
            return Err(DaemonError::new(
                ErrorCode::InvalidInput,
                "request",
                "request too large",
            ));
        }
        bytes.push(b'\n');
        stream
            .write_all(&bytes)
            .await
            .map_err(|e| DaemonError::io("request write", e))?;
        dispatched = true;
        let mut reader = tokio::io::BufReader::new(&mut stream);
        let bytes = read_response_frame(&mut reader).await?.ok_or_else(|| {
            DaemonError::new(
                ErrorCode::ServiceUnavailable,
                "response",
                "daemon closed before replying",
            )
        })?;
        let response: Response = serde_json::from_slice(&bytes)
            .map_err(|e| DaemonError::new(ErrorCode::MalformedFrame, "response", e.to_string()))?;
        response.validate(request.id)?;
        Ok(response)
    })
    .await
    .map_err(|_| {
        DaemonError::new(
            if mutation && dispatched {
                ErrorCode::OutcomeUnknown
            } else {
                ErrorCode::CommandTimeout
            },
            "request",
            "request deadline elapsed; inspect status before retrying a dispatched mutation",
        )
    })?
    .map_err(|error| {
        if mutation && dispatched && error.code != ErrorCode::OutcomeUnknown {
            DaemonError::new(
                ErrorCode::OutcomeUnknown,
                error.stage,
                format!(
                    "response unavailable after dispatch ({:?}): {}; inspect status before retrying",
                    error.code,
                    error.message
                ),
            )
        } else {
            error
        }
    })
}

pub async fn watch_status(
    paths: &Paths,
    request: &Request,
    mut output: impl FnMut(Snapshot),
) -> Result<(), DaemonError> {
    let mut stream =
        tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(paths.socket()))
            .await
            .map_err(|_| {
                DaemonError::new(
                    ErrorCode::ServiceUnavailable,
                    "watch",
                    "daemon connection timed out",
                )
            })?
            .map_err(|e| DaemonError::io("watch connect", e))?;
    let mut bytes = serde_json::to_vec(request)
        .map_err(|e| DaemonError::new(ErrorCode::InvalidInput, "watch", e.to_string()))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| DaemonError::io("watch write", e))?;
    let mut reader = tokio::io::BufReader::new(&mut stream);
    while let Some(bytes) = read_response_frame(&mut reader).await? {
        let response: Response = serde_json::from_slice(&bytes)
            .map_err(|e| DaemonError::new(ErrorCode::MalformedFrame, "watch", e.to_string()))?;
        response.validate(request.id)?;
        if let Some(error) = response.error {
            return Err(error);
        }
        let snapshot = serde_json::from_value(response.result.ok_or_else(|| {
            DaemonError::new(ErrorCode::MalformedFrame, "watch", "missing status")
        })?)
        .map_err(|e| DaemonError::new(ErrorCode::MalformedFrame, "watch", e.to_string()))?;
        output(snapshot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn framing_preserves_split_and_combined_messages() {
        let (mut left, mut right) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            left.write_all(b"one\nsecond\n").await.expect("write");
        });
        assert_eq!(
            read_frame(&mut right).await.expect("frame"),
            Some(b"one".to_vec())
        );
        assert_eq!(
            read_frame(&mut right).await.expect("frame"),
            Some(b"second".to_vec())
        );
        writer.await.expect("writer");
        assert!(read_frame(&mut right).await.expect("eof").is_none());
    }
    #[tokio::test]
    async fn framing_rejects_partial_eof_and_oversize() {
        let mut partial: &[u8] = b"unfinished";
        assert!(read_frame(&mut partial).await.is_err());
        let bytes = vec![b'a'; ipc::MAX_FRAME + 1];
        let mut oversized = bytes.as_slice();
        assert!(read_frame(&mut oversized).await.is_err());
    }
}
