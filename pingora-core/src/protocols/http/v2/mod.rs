// Copyright 2025 Cloudflare, Inc.
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

use crate::{Error, ErrorType::*, OrErr, Result};
use bytes::Bytes;
use h2::SendStream;

pub mod client;
pub mod server;

/// A helper function to write the body of h2 streams.
pub async fn write_body(writer: &mut SendStream<Bytes>, data: Bytes, end: bool) -> Result<()> {
    let mut remaining = data;

    // Cannot poll 0 capacity, so send it directly.
    if remaining.is_empty() {
        writer
            .send_data(remaining, end)
            .or_err(WriteError, "while writing h2 request body")?;
        return Ok(());
    }

    loop {
        writer.reserve_capacity(remaining.len());
        match std::future::poll_fn(|cx| writer.poll_capacity(cx)).await {
            None => return Error::e_explain(H2Error, "cannot reserve capacity"),
            Some(ready) => {
                let n = ready.or_err(H2Error, "while waiting for capacity")?;
                let remaining_size = remaining.len();
                let data_to_send = remaining.split_to(std::cmp::min(remaining_size, n));
                writer
                    .send_data(data_to_send, remaining.is_empty() && end)
                    .or_err(WriteError, "while writing h2 request body")?;
                if remaining.is_empty() {
                    return Ok(());
                }
            }
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
    use tokio_stream::StreamExt;

    use pingora_http::{RequestHeader, ResponseHeader};
    use pingora_timeout::sleep;

    use crate::protocols::{
        http::v2::server::{handshake, HttpSession},
        Digest,
    };

    #[tokio::test]
    async fn test_client_write_timeout() {
        let mut handles = vec![];

        let (client, mut server) = duplex(65536);

        // Client
        handles.push(tokio::spawn(async move {
            let conn = crate::connectors::http::v2::handshake(Box::new(client), 500, None)
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
}
