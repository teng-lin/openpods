// SPDX-License-Identifier: AGPL-3.0-only
#![cfg(target_os = "linux")]
use librepods_daemon::{
    ipc::{Response, MAX_FRAME, MAX_RESPONSE_FRAME},
    runtime::{read_frame, read_response_frame, write_response},
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
    let (mut writer, mut reader) = tokio::io::duplex(1024);
    let oversized = Response::ok(1, "x".repeat(MAX_RESPONSE_FRAME));
    write_response(&mut writer, &oversized)
        .await
        .expect("typed bounded error written");
    let response: Response = serde_json::from_slice(
        &read_response_frame(&mut reader)
            .await
            .expect("error frame")
            .expect("response"),
    )
    .expect("response schema");
    assert_eq!(response.id, 1);
    assert_eq!(
        response.error.expect("oversized response error").code,
        ErrorCode::ResponseTooLarge
    );
    assert!(response.result.is_none());
    let (mut writer, _reader) = tokio::io::duplex(1);
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

#[tokio::test]
async fn response_frames_accept_exact_response_bound_and_reject_one_extra_byte() {
    for length in [MAX_RESPONSE_FRAME, MAX_RESPONSE_FRAME + 1] {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let sent = tokio::spawn(async move {
            let mut bytes = vec![b'x'; length];
            bytes.push(b'\n');
            let _ = writer.write_all(&bytes).await;
        });
        let result = read_response_frame(&mut reader).await;
        drop(reader);
        sent.await.expect("writer task");
        if length == MAX_RESPONSE_FRAME {
            assert_eq!(
                result
                    .expect("exact response limit")
                    .expect("response")
                    .len(),
                length
            );
        } else {
            assert_eq!(
                result.expect_err("oversized response").code,
                ErrorCode::InvalidInput
            );
        }
    }
}
