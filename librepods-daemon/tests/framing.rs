// SPDX-License-Identifier: AGPL-3.0-only
#![cfg(target_os = "linux")]
use librepods_daemon::{
    ipc::{Response, MAX_FRAME},
    runtime::{read_frame, write_response},
    ErrorCode,
};
use tokio::io::AsyncWriteExt;

async fn receive(bytes: Vec<u8>) -> Result<Option<Vec<u8>>, librepods_daemon::DaemonError> {
    let (mut writer, mut reader) = tokio::io::duplex(1024);
    let sent = tokio::spawn(async move {
        let _ = writer.write_all(&bytes).await;
    });
    let result = read_frame(&mut reader).await;
    drop(reader);
    sent.await.expect("writer task");
    result
}

#[tokio::test]
async fn frame_limit_is_exact_and_partial_eof_is_distinct_from_clean_close() {
    assert_eq!(receive(vec![]).await.expect("clean EOF"), None);
    assert_eq!(
        receive(b"unterminated".to_vec())
            .await
            .expect_err("partial EOF")
            .code,
        ErrorCode::MalformedFrame
    );
    let mut exact = vec![b'x'; MAX_FRAME];
    exact.push(b'\n');
    assert_eq!(
        receive(exact)
            .await
            .expect("exact limit")
            .expect("frame")
            .len(),
        MAX_FRAME
    );
    assert_eq!(
        receive(vec![b'x'; MAX_FRAME + 1])
            .await
            .expect_err("oversized frame")
            .code,
        ErrorCode::InvalidInput
    );
}

#[tokio::test]
async fn response_size_and_nonreading_peer_have_bounded_distinct_errors() {
    let (mut writer, _reader) = tokio::io::duplex(1);
    let oversized = Response::ok(1, "x".repeat(MAX_FRAME));
    assert_eq!(
        write_response(&mut writer, &oversized)
            .await
            .expect_err("oversize")
            .code,
        ErrorCode::InvalidInput
    );
    let started = std::time::Instant::now();
    assert_eq!(
        write_response(&mut writer, &Response::ok(2, "small"))
            .await
            .expect_err("stalled reader")
            .code,
        ErrorCode::CommandTimeout
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let (mut writer, reader) = tokio::io::duplex(1);
    drop(reader);
    assert!(write_response(&mut writer, &Response::ok(3, "small"))
        .await
        .is_err());
}
