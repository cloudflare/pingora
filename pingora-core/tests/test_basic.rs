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

mod utils;

#[cfg(all(unix, feature = "any_tls"))]
use hyperlocal::{UnixClientExt, Uri};
use log::{debug, info};
use pingora_error::{ErrorType, OrErr, Result};
use std::time::{Duration, Instant};

use h3i::actions::h3::send_headers_frame;
use h3i::actions::h3::Action;
use h3i::actions::h3::StreamEvent;
use h3i::actions::h3::StreamEventType;
use h3i::actions::h3::WaitType;
use h3i::client::sync_client;
use h3i::config::Config;
use h3i::frame::H3iFrame;
use h3i::quiche::h3::frame::Frame;
use h3i::quiche::h3::Header;

#[tokio::test]
async fn test_http() {
    utils::init();
    let res = reqwest::get("http://127.0.0.1:6145").await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_https_http2() {
    utils::init();

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client.get("https://127.0.0.1:6146").send().await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http1_only()
        .build()
        .unwrap();

    let res = client.get("https://127.0.0.1:6146").send().await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_11);
}

#[cfg(unix)]
#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_uds() {
    utils::init();
    let url = Uri::new("/tmp/echo.sock", "/").into();
    let client = hyper::Client::unix();

    let res = client.get(url).await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn test_listener_quic_http3() -> Result<()> {
    utils::init();
    info!("Startup completed..");

    let host = "openrusty.org";
    let config = Config::new()
        .with_connect_to("127.0.0.1:6147".to_string())
        .with_host_port(format!("{}:6147", host))
        .with_idle_timeout(2000)
        .verify_peer(false)
        .build()
        .unwrap();

    let body = b"test".to_vec();
    let headers = vec![
        Header::new(b":method", b"POST"),
        Header::new(b":scheme", b"https"),
        Header::new(b":authority", host.as_bytes()),
        Header::new(b":path", b"/"),
        Header::new(b"content-length", body.len().to_string().as_bytes()),
    ];
    const STREAM_ID: u64 = 0;
    let actions = vec![
        send_headers_frame(STREAM_ID, false, headers),
        Action::SendFrame {
            stream_id: STREAM_ID,
            fin_stream: true,
            frame: Frame::Data {
                payload: body.clone(),
            },
        },
        Action::Wait {
            wait_type: WaitType::StreamEvent(StreamEvent {
                stream_id: STREAM_ID,
                event_type: StreamEventType::Finished,
            }),
        },
        Action::ConnectionClose {
            error: quiche::ConnectionError {
                is_app: true,
                error_code: quiche::h3::WireErrorCode::NoError as u64,
                reason: vec![],
            },
        },
    ];

    let summary = sync_client::connect(config, &actions, None)
        .explain_err(ErrorType::H3Error, |e| format!("connection failed {:?}", e))?;

    debug!("summary: {:?}", &summary);

    let stream = summary.stream_map.stream(STREAM_ID);
    let resp_headers = stream
        .iter()
        .find(|e| matches!(e, H3iFrame::Headers(..)))
        .unwrap();
    let resp_body: Vec<Vec<u8>> = stream
        .iter()
        .filter_map(|f| match f {
            H3iFrame::QuicheH3(Frame::Data { payload }) => Some(payload.clone()),
            _ => None,
        })
        .collect();

    debug!(
        "response headers: {:?}, body: {:?} ",
        &resp_headers,
        String::from_utf8(resp_body[0].clone())
    );
    let headers = resp_headers.to_enriched_headers().unwrap();
    let headers = headers.header_map();
    let status = headers.get(b":status".as_slice()).unwrap();
    let content_type = headers.get(b"content-type".as_slice()).unwrap();
    let content_length = headers.get(b"content-length".as_slice()).unwrap();
    assert_eq!(status, &b"200".to_vec());
    assert_eq!(content_type, &b"text/html".to_vec());
    assert_eq!(content_length, &body.len().to_string().as_bytes().to_vec());
    assert_eq!(resp_body[0], body.as_slice().to_vec());
    Ok(())
}

#[tokio::test]
async fn test_listener_quic_http3_timeout() -> Result<()> {
    utils::init();
    info!("Startup completed..");

    let host = "openrusty.org";
    let config = Config::new()
        .with_connect_to("127.0.0.1:6147".to_string())
        .with_host_port(format!("{}:6147", host))
        .with_idle_timeout(3000)
        .verify_peer(false)
        .build()
        .unwrap();

    let body = b"test".to_vec();
    let headers = vec![
        Header::new(b":method", b"POST"),
        Header::new(b":scheme", b"https"),
        Header::new(b":authority", host.as_bytes()),
        Header::new(b":path", b"/"),
        Header::new(b"content-length", body.len().to_string().as_bytes()),
    ];
    const STREAM_ID: u64 = 0;
    let actions = vec![
        send_headers_frame(STREAM_ID, false, headers),
        Action::SendFrame {
            stream_id: STREAM_ID,
            fin_stream: true,
            frame: Frame::Data {
                payload: body.clone(),
            },
        },
        Action::Wait {
            wait_type: WaitType::StreamEvent(StreamEvent {
                stream_id: STREAM_ID,
                event_type: StreamEventType::Finished,
            }),
        },
    ];

    let now = Instant::now();
    let summary = sync_client::connect(config.clone(), &actions, None)
        .explain_err(ErrorType::H3Error, |e| format!("connection failed {:?}", e))?;
    let runtime = now.elapsed();

    assert!(runtime >= Duration::from_millis(config.idle_timeout));
    assert!(runtime < Duration::from_millis(config.idle_timeout + 100));

    let stream = summary.stream_map.stream(STREAM_ID);
    let resp_headers = stream
        .iter()
        .find(|e| matches!(e, H3iFrame::Headers(..)))
        .unwrap()
        .to_enriched_headers()
        .unwrap();
    let status = resp_headers.status_code().unwrap();
    assert_eq!(status, &b"200".to_vec());
    Ok(())
}
