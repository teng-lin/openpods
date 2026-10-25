// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded native Unix IPC framing and client deadlines, shared with the daemon.
use crate::{
    ipc::{self, Request, Response},
    DaemonError, ErrorCode,
};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
};
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

pub async fn request(socket: &std::path::Path, request: &Request) -> Result<Response, DaemonError> {
    let duration = Duration::from_millis(request.timeout_ms);
    let mutation = request.operation.is_mutation();
    let mut dispatched = false;
    tokio::time::timeout(duration, async {
        let mut stream = UnixStream::connect(socket)
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
