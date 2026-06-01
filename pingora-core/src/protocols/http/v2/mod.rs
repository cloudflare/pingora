// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! HTTP/2 implementation

use std::time::Duration;

use crate::{Error, ErrorType::*, OrErr, Result};
use pingora_timeout::timeout;

use bytes::Bytes;
use h2::SendStream;

pub mod client;
pub mod server;

async fn reserve_and_send(
    writer: &mut SendStream<Bytes>,
    remaining: &mut Bytes,
    end: bool,
) -> Result<()> {
    // reserve remaining bytes then wait
    writer.reserve_capacity(remaining.len());
    let res = std::future::poll_fn(|cx| writer.poll_capacity(cx)).await;

    match res {
        None => Error::e_explain(H2Error, "cannot reserve capacity"),
        Some(ready) => {
            let n = ready.or_err(H2Error, "while waiting for capacity")?;
            let remaining_size = remaining.len();
            let data_to_send = remaining.split_to(std::cmp::min(remaining_size, n));
            writer
                .send_data(data_to_send, remaining.is_empty() && end)
                .or_err(WriteError, "while writing h2 request body")?;
            Ok(())
        }
    }
}

/// A helper function to write the body of h2 streams.
pub async fn write_body(
    writer: &mut SendStream<Bytes>,
    data: Bytes,
    end: bool,
    write_timeout: Option<Duration>,
) -> Result<()> {
    let mut remaining = data;

    // Cannot poll 0 capacity, so send it directly.
    if remaining.is_empty() {
        writer
            .send_data(remaining, end)
            .or_err(WriteError, "while writing h2 request body")?;
        return Ok(());
    }

    loop {
        match write_timeout {
            Some(t) => match timeout(t, reserve_and_send(writer, &mut remaining, end)).await {
                Ok(res) => res?,
                Err(_) => Error::e_explain(
                    WriteTimedout,
                    format!("while writing h2 request body, timeout: {t:?}"),
                )?,
            },
            None => {
                reserve_and_send(writer, &mut remaining, end).await?;
            }
        }
        if remaining.is_empty() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod test {
    use std::{sync::Arc, time::Duration};

    use bytes::Bytes;
    use futures::SinkExt;
    use h2::frame::*;
    use http::{HeaderMap, Method, Uri};
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::sync::{oneshot, watch};
    use tokio_stream::StreamExt;

    use pingora_http::{RequestHeader, ResponseHeader};
    use pingora_timeout::sleep;

    use crate::protocols::{
        http::v2::server::{self, handshake, HttpSession},
        Digest,
    };

    #[tokio::test]
    async fn test_client_write_timeout() {
        let mut handles = vec![];

        let (client, mut server) = duplex(65536);

        // Client
        handles.push(tokio::spawn(async move {
            use crate::connectors::http::v2::H2HandshakeSettings;
            let mut settings = H2HandshakeSettings::new();
            settings.max_streams = 500;
            let conn = crate::connectors::http::v2::handshake(Box::new(client), settings)
                .await
                .unwrap();

            let mut h2_stream = conn.spawn_stream().await.unwrap().unwrap();
            h2_stream.write_timeout = Some(Duration::from_millis(100));

            let mut request = RequestHeader::build("GET", b"/", None).unwrap();
            request.insert_header("Host", "one.one.one.one").unwrap();

            h2_stream
                .write_request_header(Box::new(request), false)
                .unwrap();

            h2_stream.read_response_header().await.unwrap();
            assert_eq!(h2_stream.response_header().unwrap().status.as_u16(), 200);

            let err = h2_stream
                .write_request_body(Bytes::from_static(b"client body"), true)
                .await
                .err()
                .unwrap();
            assert_eq!(err.etype(), &pingora_error::ErrorType::WriteTimedout);
        }));

        // Server
        handles.push(tokio::spawn(async move {
            // 0. Prepare outbound frames
            let mut outbound: Vec<h2::frame::Frame<Bytes>> = Vec::new();

            let mut settings = Settings::default();

            settings.set_initial_window_size(Some(1));
            settings.set_max_concurrent_streams(Some(1));

            outbound.push(settings.into());
            outbound.push(Settings::ack().into());

            let headers = HeaderMap::new();

            outbound.push(
                Headers::new(1.into(), Pseudo::response(http::StatusCode::OK), headers).into(),
            );

            outbound.push(WindowUpdate::new(1.into(), 10000).into());

            // 1. Read preface from the client
            server.read_exact(&mut [0u8; 24]).await.unwrap();

            let mut server: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(server);

            // 2. Drain client's frames
            for _ in 0..3 {
                _ = server.next().await.unwrap();
            }

            // 3. Send frames
            for (i, frame) in outbound.into_iter().enumerate() {
                if i == 3 {
                    // Delay WindowUpdate to trigger client side write timeout on capacity await
                    sleep(Duration::from_millis(200)).await;
                }
                _ = server.send(frame).await;
            }
        }));

        for handle in handles {
            // ensure no panics
            assert!(handle.await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_server_write_timeout() {
        let mut handles = vec![];

        let (mut client, server) = duplex(65536);

        // Client
        handles.push(tokio::spawn(async move {
            // 0. Prepare outbound frames
            let mut outbound: Vec<h2::frame::Frame<Bytes>> = Vec::new();

            let mut settings = Settings::default();

            settings.set_initial_window_size(Some(1));
            settings.set_max_concurrent_streams(Some(1));
            outbound.push(settings.into());

            outbound.push(Settings::ack().into());

            let mut headers = Headers::new(
                1.into(),
                Pseudo::request(
                    Method::GET,
                    Uri::from_static("https://one.one.one.one"),
                    None,
                ),
                HeaderMap::new(),
            );
            headers.set_end_headers();
            outbound.push(headers.into());

            outbound.push(WindowUpdate::new(1.into(), 10000).into());

            // 1. Write h2 preface
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();

            // 2. Send frames
            let mut client: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);

            for (i, frame) in outbound.into_iter().enumerate() {
                if i == 3 {
                    // Delay WindowUpdate to trigger server side write timeout on capacity await
                    sleep(Duration::from_millis(200)).await;
                }
                _ = client.send(frame).await;
            }

            // 3. Drain server's frames
            for _ in 0..3 {
                _ = client.next().await.unwrap();
            }
        }));

        // Server
        let mut connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        while let Some(mut h2_stream) = HttpSession::from_h2_conn(&mut connection, digest.clone())
            .await
            .unwrap()
        {
            handles.push(tokio::spawn(async move {
                h2_stream.set_write_timeout(Some(Duration::from_millis(100)));
                let req = h2_stream.req_header();
                assert_eq!(req.method, Method::GET);

                let response_header = Box::new(ResponseHeader::build(200, None).unwrap());
                assert!(h2_stream
                    .write_response_header(response_header.clone(), false)
                    .is_ok());

                let err = h2_stream
                    .write_body(Bytes::from_static(b"server body"), true)
                    .await
                    .err()
                    .unwrap();
                assert_eq!(err.etype(), &pingora_error::ErrorType::WriteTimedout);
            }));
        }

        for handle in handles {
            // ensure no panics
            assert!(handle.await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_graceful_shutdown_processes_inflight_stream() {
        // HEADERS arrive on the server after the shutdown signal
        // fires, but before the client has observed GOAWAY.
        let (mut client, server) = duplex(65536);
        // Use channels for deterministic timing.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (write_headers_tx, write_headers_rx) = oneshot::channel::<()>();

        let client_handle = tokio::spawn(async move {
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();
            let mut codec: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);
            codec.send(Settings::default().into()).await.unwrap();
            codec.send(Settings::ack().into()).await.unwrap();

            // Wait until the test has triggered shutdown on the server before
            // sending HEADERS. See the function-level comment for why.
            write_headers_rx.await.unwrap();

            let mut headers = Headers::new(
                1.into(),
                Pseudo::request(
                    Method::GET,
                    Uri::from_static("https://one.one.one.one/"),
                    None,
                ),
                HeaderMap::new(),
            );
            headers.set_end_headers();
            headers.set_end_stream();
            codec.send(headers.into()).await.unwrap();

            let mut saw_response = false;
            let mut saw_goaway = false;
            let _ = pingora_timeout::timeout(Duration::from_secs(5), async {
                while let Some(frame) = codec.next().await {
                    match frame.unwrap() {
                        h2::frame::Frame::Headers(_) => {
                            saw_response = true;
                        }
                        h2::frame::Frame::GoAway(_) => {
                            saw_goaway = true;
                        }
                        _ => {}
                    }
                    if saw_response && saw_goaway {
                        break;
                    }
                }
            })
            .await;

            assert!(saw_response, "expected response for stream 1");
            assert!(saw_goaway, "expected at least one GOAWAY frame");
        });

        let connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        let trigger = tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            shutdown_tx.send(true).unwrap();
            // Wait long enough that the server task is guaranteed to have
            // observed the shutdown signal and committed to its post-shutdown
            // path before putting anything on the wire.
            sleep(Duration::from_millis(50)).await;
            write_headers_tx.send(()).unwrap();
        });

        let mut session_handles = vec![];
        server::accept_downstream_sessions(
            connection,
            digest,
            shutdown_rx,
            None,
            |mut session, _guard| {
                session_handles.push(tokio::spawn(async move {
                    let req = session.req_header();
                    assert_eq!(req.method, Method::GET);
                    let resp = Box::new(ResponseHeader::build(200, None).unwrap());
                    session.write_response_header(resp, true).unwrap();
                }));
            },
        )
        .await;

        trigger.await.unwrap();
        assert_eq!(
            session_handles.len(),
            1,
            "expected stream 1 to be surfaced after shutdown_initiated"
        );
        for h in session_handles {
            h.await.unwrap();
        }
        client_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_graceful_shutdown_processes_post_goaway_stream() {
        // Client opens stream 1 after it has observed the
        // server's GOAWAY frame. Stream 1 is below the GOAWAY(MAX)
        // last_stream_id, so per RFC 9113 §6.8 the server must still process
        // it.
        let (mut client, server) = duplex(65536);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let client_handle = tokio::spawn(async move {
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();
            let mut codec: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);
            codec.send(Settings::default().into()).await.unwrap();
            codec.send(Settings::ack().into()).await.unwrap();

            // Block until the server's GOAWAY is observed. HEADERS for
            // stream 1 are sent strictly after this point.
            let mut saw_goaway_before_headers = false;
            while let Some(frame) = codec.next().await {
                if matches!(frame.unwrap(), h2::frame::Frame::GoAway(_)) {
                    saw_goaway_before_headers = true;
                    break;
                }
            }
            assert!(
                saw_goaway_before_headers,
                "expected GOAWAY before sending HEADERS for stream 1"
            );

            let mut headers = Headers::new(
                1.into(),
                Pseudo::request(
                    Method::GET,
                    Uri::from_static("https://one.one.one.one/"),
                    None,
                ),
                HeaderMap::new(),
            );
            headers.set_end_headers();
            headers.set_end_stream();
            codec.send(headers.into()).await.unwrap();

            let mut saw_response_for_stream_1 = false;
            let _ = pingora_timeout::timeout(Duration::from_secs(5), async {
                while let Some(frame) = codec.next().await {
                    if let Ok(h2::frame::Frame::Headers(h)) = frame {
                        if h.stream_id() == 1u32 {
                            saw_response_for_stream_1 = true;
                            break;
                        }
                    }
                }
            })
            .await;
            assert!(
                saw_response_for_stream_1,
                "expected response on stream 1 after GOAWAY",
            );
        });

        let connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        let trigger = tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            shutdown_tx.send(true).unwrap();
        });

        let mut session_handles = vec![];
        server::accept_downstream_sessions(
            connection,
            digest,
            shutdown_rx,
            None,
            |mut session, _guard| {
                session_handles.push(tokio::spawn(async move {
                    let resp = Box::new(ResponseHeader::build(200, None).unwrap());
                    session.write_response_header(resp, true).unwrap();
                }));
            },
        )
        .await;

        trigger.await.unwrap();
        assert_eq!(
            session_handles.len(),
            1,
            "expected exactly one stream surfaced after GOAWAY"
        );
        for h in session_handles {
            h.await.unwrap();
        }
        client_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_graceful_shutdown_idle_connection_exits_promptly() {
        let (mut client, server) = duplex(65536);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let client_handle = tokio::spawn(async move {
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();
            let mut codec: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);
            codec.send(Settings::default().into()).await.unwrap();
            codec.send(Settings::ack().into()).await.unwrap();

            // Wait for the server's GOAWAY, then drop the codec to close the
            // connection so the accept loop can exit.
            let mut saw_goaway = false;
            let _ = pingora_timeout::timeout(Duration::from_secs(3), async {
                while let Some(frame) = codec.next().await {
                    if matches!(frame.unwrap(), h2::frame::Frame::GoAway(_)) {
                        saw_goaway = true;
                        break;
                    }
                }
            })
            .await;
            assert!(saw_goaway, "expected GOAWAY");
        });

        let connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        let trigger = tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            shutdown_tx.send(true).unwrap();
        });

        let result = pingora_timeout::timeout(
            Duration::from_secs(2),
            server::accept_downstream_sessions(
                connection,
                digest,
                shutdown_rx,
                None,
                |_session, _guard| {
                    panic!("did not expect any sessions on an idle connection");
                },
            ),
        )
        .await;
        assert!(result.is_ok(), "accept loop hung after shutdown");

        trigger.await.unwrap();
        client_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_h2_idle_timeout_closes_idle_connection() {
        let (mut client, server) = duplex(65536);
        // Keep the sender alive so `shutdown.changed()` stays pending — the only
        // thing that should end the accept loop is the idle timeout.
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let client_handle = tokio::spawn(async move {
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();
            let mut codec: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);
            codec.send(Settings::default().into()).await.unwrap();
            codec.send(Settings::ack().into()).await.unwrap();
            // Open no streams; stay connected and drain frames until the server
            // drops the connection on idle timeout (codec returns None on EOF).
            while let Some(frame) = codec.next().await {
                let _ = frame;
            }
        });

        let connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        // No shutdown is signaled and no stream is opened; a short idle timeout
        // must make the accept loop return on its own.
        let result = pingora_timeout::timeout(
            Duration::from_secs(2),
            server::accept_downstream_sessions(
                connection,
                digest,
                shutdown_rx,
                Some(Duration::from_millis(100)),
                |_session, _guard| panic!("did not expect any sessions on an idle connection"),
            ),
        )
        .await;
        assert!(result.is_ok(), "idle timeout did not close the connection");

        client_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_graceful_shutdown_refuses_stream_above_last_stream_id() {
        // After the server commits to a final last_stream_id and emits the
        // closing GOAWAY, any stream the client tries to open above that id
        // must be refused. The accept loop must not surface it and must exit
        // cleanly via the `Ok(None)` arm of `from_h2_conn`.
        let (mut client, server) = duplex(65536);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let client_handle = tokio::spawn(async move {
            client
                .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();
            let mut codec: h2::Codec<DuplexStream, Bytes> = h2::Codec::new(client);
            codec.send(Settings::default().into()).await.unwrap();
            codec.send(Settings::ack().into()).await.unwrap();

            // Open stream 1 so the server will commit last_stream_id >= 1
            // when it eventually emits its closing GOAWAY.
            let mut headers = Headers::new(
                1.into(),
                Pseudo::request(
                    Method::GET,
                    Uri::from_static("https://one.one.one.one"),
                    None,
                ),
                HeaderMap::new(),
            );
            headers.set_end_headers();
            headers.set_end_stream();
            codec.send(headers.into()).await.unwrap();

            // Drain frames from the server. Break once we've seen the
            // response and at least one GOAWAY so the test doesn't race
            // its own outer timeout while waiting on a quiet codec.
            let mut saw_response = false;
            let mut saw_goaway = false;
            let _ = pingora_timeout::timeout(Duration::from_secs(3), async {
                while let Some(frame) = codec.next().await {
                    match frame {
                        Ok(h2::frame::Frame::Headers(h)) if h.stream_id() == 1 => {
                            saw_response = true;
                        }
                        Ok(h2::frame::Frame::GoAway(_)) => {
                            saw_goaway = true;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if saw_response && saw_goaway {
                        break;
                    }
                }
            })
            .await;
            assert!(saw_response, "expected response for stream 1");
            assert!(saw_goaway, "expected at least one GOAWAY frame");

            // Try to open stream 3 (above last_stream_id). The send may
            // succeed locally (duplex buffer) or fail (peer half closed);
            // either way the server-side codec must not surface the stream.
            let mut headers = Headers::new(
                3.into(),
                Pseudo::request(
                    Method::GET,
                    Uri::from_static("https://one.one.one.one"),
                    None,
                ),
                HeaderMap::new(),
            );
            headers.set_end_headers();
            headers.set_end_stream();
            let _ = codec.send(headers.into()).await;
        });

        let connection = handshake(Box::new(server), None).await.unwrap();
        let digest = Arc::new(Digest::default());

        let trigger = tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            shutdown_tx.send(true).unwrap();
        });

        let mut session_handles = vec![];
        let result = pingora_timeout::timeout(
            Duration::from_secs(5),
            server::accept_downstream_sessions(
                connection,
                digest,
                shutdown_rx,
                None,
                |mut session, _guard| {
                    session_handles.push(tokio::spawn(async move {
                        let resp = Box::new(ResponseHeader::build(200, None).unwrap());
                        session.write_response_header(resp, true).unwrap();
                    }));
                },
            ),
        )
        .await;
        assert!(result.is_ok(), "accept loop hung after shutdown");
        assert_eq!(
            session_handles.len(),
            1,
            "only stream 1 may be surfaced; streams above last_stream_id must be refused"
        );

        trigger.await.unwrap();
        for h in session_handles {
            h.await.unwrap();
        }
        client_handle.await.unwrap();
    }
}
