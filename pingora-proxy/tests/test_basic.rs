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

mod utils;

use bytes::Bytes;
use h2::client;
use http::Request;
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
#[cfg(unix)]
use hyperlocal::{UnixClientExt, Uri};
use reqwest::{header, StatusCode};
#[cfg(feature = "patched_http1")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use utils::server_utils::{
    downstream_cache_warn_log_calls, init, reset_suppress_proxy_warn_log_calls,
    suppress_proxy_warn_log_calls,
};

fn is_specified_port(port: u16) -> bool {
    (1..65535).contains(&port)
}

#[tokio::test]
async fn test_origin_alive() {
    init();
    let res = reqwest::get("http://127.0.0.1:8000/").await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}

#[tokio::test]
async fn test_simple_proxy() {
    init();
    let res = reqwest::get("http://127.0.0.1:6147").await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    assert_eq!(headers["x-server-addr"], "127.0.0.1:6147");
    let sockaddr = headers["x-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.1");
    assert!(is_specified_port(sockaddr.port()));

    assert_eq!(headers["x-upstream-server-addr"], "127.0.0.1:8000");
    let sockaddr = headers["x-upstream-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.2");
    assert!(is_specified_port(sockaddr.port()));

    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}

#[tokio::test]
#[cfg(feature = "any_tls")]
async fn test_h2_to_h1() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .get("https://127.0.0.1:6150")
        .header("sni", "openrusty.org")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);

    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    assert_eq!(headers["x-server-addr"], "127.0.0.1:6150");

    let sockaddr = headers["x-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.1");
    assert!(is_specified_port(sockaddr.port()));

    assert_eq!(headers["x-upstream-server-addr"], "127.0.0.1:8443");
    let sockaddr = headers["x-upstream-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.2");
    assert!(is_specified_port(sockaddr.port()));

    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}

#[tokio::test]
#[cfg(feature = "any_tls")]
async fn test_h2_to_h2() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .get("https://127.0.0.1:6150")
        .header("sni", "openrusty.org")
        .header("x-h2", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);

    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    assert_eq!(headers["x-server-addr"], "127.0.0.1:6150");
    let sockaddr = headers["x-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.1");
    assert!(is_specified_port(sockaddr.port()));

    assert_eq!(headers["x-upstream-server-addr"], "127.0.0.1:8443");
    let sockaddr = headers["x-upstream-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.2");
    assert!(is_specified_port(sockaddr.port()));

    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}

#[tokio::test]
async fn test_h2c_to_h2c() {
    init();

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .http2_only(true)
        .build_http::<http_body_util::Empty<Bytes>>();

    let mut req = http::Request::builder()
        .uri("http://127.0.0.1:6146")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    req.headers_mut()
        .insert("x-h2", http::HeaderValue::from_bytes(b"true").unwrap());
    let res = client.request(req).await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);

    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"Hello World!\n");
}

#[tokio::test]
async fn test_h1_on_h2c_port() {
    init();

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .http2_only(false)
        .build_http::<http_body_util::Empty<Bytes>>();

    let mut req = http::Request::builder()
        .uri("http://127.0.0.1:6146")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    req.headers_mut()
        .insert("x-h2", http::HeaderValue::from_bytes(b"true").unwrap());
    let res = client.request(req).await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_11);

    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"Hello World!\n");
}

#[tokio::test]
#[cfg(feature = "openssl_derived")]
async fn test_h2_to_h2_host_override() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .get("https://127.0.0.1:6150")
        .header("x-h2", "true")
        .header("host-override", "test.com")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);
    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}

#[tokio::test]
#[cfg(feature = "any_tls")]
async fn test_h2_to_h2_upload() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let payload = "test upload";

    let res = client
        .get("https://127.0.0.1:6150/echo")
        .header("sni", "openrusty.org")
        .header("x-h2", "true")
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);
    let body = res.text().await.unwrap();
    assert_eq!(body, payload);
}

#[tokio::test]
#[cfg(feature = "any_tls")]
async fn test_h2_to_h1_upload() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let payload = "test upload";

    let res = client
        .get("https://127.0.0.1:6150/echo")
        .header("sni", "openrusty.org")
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);
    let body = res.text().await.unwrap();
    assert_eq!(body, payload);
}

#[tokio::test]
#[cfg(feature = "any_tls")]
async fn test_h2_head() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .head("https://127.0.0.1:6150/set_content_length")
        .header("sni", "openrusty.org")
        .header("x-h2", "true")
        .header("x-set-content-length", "11")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);
    let body = res.text().await.unwrap();
    // should not be any body, despite content-length
    assert_eq!(body, "");
}

#[cfg(unix)]
#[tokio::test]
async fn test_simple_proxy_uds() {
    init();
    let url = Uri::new("/tmp/pingora_proxy.sock", "/").into();
    let client: Client<hyperlocal::UnixConnector, http_body_util::Empty<Bytes>> = Client::unix();

    let res = client.get(url).await.unwrap();

    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let (resp, body) = res.into_parts();

    let headers = &resp.headers;
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    assert_eq!(headers["x-server-addr"], "/tmp/pingora_proxy.sock");
    assert_eq!(headers["x-client-addr"], "unset"); // unnamed UDS

    assert_eq!(headers["x-upstream-server-addr"], "127.0.0.1:8000");
    let sockaddr = headers["x-upstream-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.2");
    assert!(is_specified_port(sockaddr.port()));

    let body = http_body_util::BodyExt::collect(body)
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(body.as_ref(), b"Hello World!\n");
}

#[cfg(unix)]
#[tokio::test]
async fn test_simple_proxy_uds_peer() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6147")
        .header("x-uds-peer", "1") // force upstream peer to be UDS
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let headers = &res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    assert_eq!(headers["x-server-addr"], "127.0.0.1:6147");
    let sockaddr = headers["x-client-addr"]
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap();
    assert_eq!(sockaddr.ip().to_string(), "127.0.0.1");
    assert!(is_specified_port(sockaddr.port()));

    assert_eq!(headers["x-upstream-client-addr"], "unset"); // unnamed UDS
    assert_eq!(
        headers["x-upstream-server-addr"],
        "/tmp/pingora_nginx_test.sock"
    );

    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}

async fn test_dropped_conn_get() {
    init();
    let client = reqwest::Client::new();
    let port = "8001"; // special port to avoid unexpected connection reuse from other tests

    for _ in 1..3 {
        // load conns into pool
        let res = client
            .get("http://127.0.0.1:6147")
            .header("x-port", port)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    reset_suppress_proxy_warn_log_calls();
    let res = client
        .get("http://127.0.0.1:6147/bad_lb")
        .header("x-port", port)
        .header("x-test-suppress-proxy-warn-log", "true")
        .send()
        .await
        .unwrap();

    // retry gives 200
    assert_eq!(res.status(), StatusCode::OK);
    assert!(suppress_proxy_warn_log_calls() > 0);
    let body = res.text().await.unwrap();
    assert_eq!(body, "dog!\n");
}

async fn test_dropped_conn_post_empty_body() {
    init();
    let client = reqwest::Client::new();
    let port = "8001"; // special port to avoid unexpected connection reuse from other tests

    for _ in 1..3 {
        // load conn into pool
        let res = client
            .get("http://127.0.0.1:6147")
            .header("x-port", port)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    let res = client
        .post("http://127.0.0.1:6147/bad_lb")
        .header("x-port", port)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = res.text().await.unwrap();
    assert_eq!(body, "dog!\n");
}

async fn test_dropped_conn_post_body() {
    init();
    let client = reqwest::Client::new();
    let port = "8001"; // special port to avoid unexpected connection reuse from other tests

    for _ in 1..3 {
        // load conn into pool
        let res = client
            .get("http://127.0.0.1:6147")
            .header("x-port", port)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    let res = client
        .post("http://127.0.0.1:6147/bad_lb")
        .header("x-port", port)
        .body("cat!")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = res.text().await.unwrap();
    assert_eq!(body, "cat!\n");
}

async fn test_dropped_conn_post_body_over() {
    init();
    let client = reqwest::Client::new();
    let port = "8001"; // special port to avoid unexpected connection reuse from other tests
    let large_body = String::from_utf8(vec![b'e'; 1024 * 64 + 1]).unwrap();

    for _ in 1..3 {
        // load conn into pool
        let res = client
            .get("http://127.0.0.1:6147")
            .header("x-port", port)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    let res = client
        .post("http://127.0.0.1:6147/bad_lb")
        .header("x-port", port)
        .body(large_body)
        .send()
        .await
        .unwrap();

    // 502, body larger than buffer limit
    assert_eq!(res.status(), StatusCode::from_u16(502).unwrap());
}

#[tokio::test]
async fn test_dropped_conn() {
    // These tests can race with each other
    // So force run them sequentially
    test_dropped_conn_get().await;
    test_dropped_conn_post_empty_body().await;
    test_dropped_conn_post_body().await;
    test_dropped_conn_post_body_over().await;
}

// currently not supported with Rustls implementation
#[cfg(feature = "openssl_derived")]
#[tokio::test]
async fn test_tls_no_verify() {
    init();
    let client = reqwest::Client::new();
    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_verify_sni_not_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("sni", "openrusty.org")
        .header("verify", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

// currently not supported with Rustls implementation
#[cfg(feature = "openssl_derived")]
#[tokio::test]
async fn test_tls_none_verify_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_verify_sni_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("sni", "openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_underscore_sub_sni_verify_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("sni", "d_g.openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_underscore_non_sub_sni_verify_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("sni", "open_rusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    let headers = res.headers();
    assert_eq!(headers[header::CONNECTION], "close");
}

#[cfg(feature = "openssl_derived")]
#[tokio::test]
async fn test_tls_alt_verify_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("sni", "open_rusty.org")
        .header("alt", "openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "openssl_derived")]
#[tokio::test]
async fn test_tls_underscore_sub_alt_verify_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("sni", "open_rusty.org")
        .header("alt", "d_g.openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_underscore_non_sub_alt_verify_host() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("sni", "open_rusty.org")
        .header("alt", "open_rusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_upstream_compression() {
    init();

    // disable reqwest gzip support to check compression headers and body
    // otherwise reqwest will decompress and strip the headers
    let client = reqwest::ClientBuilder::new().gzip(false).build().unwrap();
    let res = client
        .get("http://127.0.0.1:6147/no_compression")
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("Content-Encoding").unwrap(), "gzip");
    let body = res.bytes().await.unwrap();
    assert!(body.len() < 32);

    // Next let reqwest decompress to validate the data
    let client = reqwest::ClientBuilder::new().gzip(true).build().unwrap();
    let res = client
        .get("http://127.0.0.1:6147/no_compression")
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.bytes().await.unwrap();
    assert_eq!(body.as_ref(), &[b'B'; 32]);
}

#[tokio::test]
async fn test_downstream_compression() {
    init();

    // disable reqwest gzip support to check compression headers and body
    // otherwise reqwest will decompress and strip the headers
    let client = reqwest::ClientBuilder::new().gzip(false).build().unwrap();
    let res = client
        .get("http://127.0.0.1:6147/no_compression")
        // tell the test proxy to use downstream compression module instead of upstream
        .header("x-downstream-compression", "1")
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("Content-Encoding").unwrap(), "gzip");
    let body = res.bytes().await.unwrap();
    assert!(body.len() < 32);

    // Next let reqwest decompress to validate the data
    let client = reqwest::ClientBuilder::new().gzip(true).build().unwrap();
    let res = client
        .get("http://127.0.0.1:6147/no_compression")
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.bytes().await.unwrap();
    assert_eq!(body.as_ref(), &[b'B'; 32]);
}

#[tokio::test]
async fn test_connect_close() {
    init();

    // default keep-alive
    let client = reqwest::ClientBuilder::new().build().unwrap();
    let res = client.get("http://127.0.0.1:6147").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    assert_eq!(headers[header::CONNECTION], "keep-alive");
    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");

    // close
    let client = reqwest::ClientBuilder::new().build().unwrap();
    let res = client
        .get("http://127.0.0.1:6147")
        .header("connection", "close")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "13");
    assert_eq!(headers[header::CONNECTION], "close");
    let body = res.text().await.unwrap();
    assert_eq!(body, "Hello World!\n");
}

// Authority-form CONNECT request targets require patched HTTP/1 parsing until
// general request-target form support is available.
#[cfg(feature = "patched_http1")]
#[tokio::test]
async fn test_connect_proxying_disallowed_h1() {
    init();

    let mut stream = TcpStream::connect("127.0.0.1:6147").await.unwrap();
    let request = b"CONNECT pingora.org:443 HTTP/1.1\r\nHost: pingora.org:443\r\n\r\n";
    stream.write_all(request).await.unwrap();

    let mut buf = [0u8; 1024];
    let read = stream.read(&mut buf).await.unwrap();
    let resp = std::str::from_utf8(&buf[..read]).unwrap();
    let status_line = resp.lines().next().unwrap_or("");
    assert!(status_line.contains(" 405 "));
}

#[tokio::test]
async fn test_connect_proxying_disallowed_h2() {
    init();

    let tcp = TcpStream::connect("127.0.0.1:6146").await.unwrap();
    let (mut h2, connection) = client::handshake(tcp).await.unwrap();
    tokio::spawn(async move {
        connection.await.unwrap();
    });

    let request = Request::builder()
        .method("CONNECT")
        .uri("http://pingora.org:443/")
        .body(())
        .unwrap();
    let (response, _body) = h2.send_request(request, true).unwrap();
    let (head, mut body) = response.await.unwrap().into_parts();
    assert_eq!(head.status.as_u16(), 405);
    while let Some(chunk) = body.data().await {
        assert!(chunk.unwrap().is_empty());
    }
}

#[cfg(feature = "patched_http1")]
#[tokio::test]
async fn test_connect_proxying_allowed_h1() {
    init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();

    // Note per RFC CONNECT 2xx responses are not allowed to have response
    // bodies, so this is non-standard behavior.
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf).await.unwrap();
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        socket.write_all(response).await.unwrap();
        let _ = socket.shutdown().await;
    });

    let mut stream = TcpStream::connect("127.0.0.1:6160").await.unwrap();
    let request = format!(
        "CONNECT pingora.org:443 HTTP/1.1\r\nHost: pingora.org:443\r\nX-Port: {}\r\n\r\n",
        upstream_addr.port()
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buf = vec![0u8; 1024];
    let read = stream.read(&mut buf).await.unwrap();
    let resp = std::str::from_utf8(&buf[..read]).unwrap();
    let status_line = resp.lines().next().unwrap_or("");
    assert!(status_line.contains(" 200 "));
    assert!(resp.ends_with("ok"));
}

#[tokio::test]
#[cfg(feature = "any_tls")]
async fn test_mtls_no_client_cert() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("x-port", "8444")
        .header("sni", "openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();

    // 400: because no cert
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_mtls_no_intermediate_cert() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/tls_verify")
        .header("x-port", "8444")
        .header("sni", "openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .header("client_cert", "1")
        .send()
        .await
        .unwrap();

    // 400: because no intermediate cert
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[cfg(feature = "any_tls")]
async fn test_mtls() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/")
        .header("x-port", "8444")
        .header("sni", "openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .header("client_cert", "1")
        .header("client_intermediate", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "any_tls")]
async fn assert_reuse(req: reqwest::RequestBuilder) {
    req.try_clone().unwrap().send().await.unwrap();
    let res = req.send().await.unwrap();
    let headers = res.headers();
    assert!(headers.get("x-conn-reuse").is_some());
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_mtls_diff_cert_no_reuse() {
    init();
    let client = reqwest::Client::new();

    let req = client
        .get("http://127.0.0.1:6149/")
        .header("x-port", "8444")
        .header("sni", "openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .header("client_cert", "1")
        .header("client_intermediate", "1");

    // pre check re-use
    assert_reuse(req).await;

    // different cert no re-use
    let res = client
        .get("http://127.0.0.1:6149/")
        .header("x-port", "8444")
        .header("sni", "openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1")
        .header("client_cert", "2")
        .header("client_intermediate", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert!(headers.get("x-conn-reuse").is_none());
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_diff_verify_no_reuse() {
    init();
    let client = reqwest::Client::new();

    let req = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "dog.openrusty.org")
        .header("verify", "1");

    // pre check re-use
    assert_reuse(req).await;

    // disable 'verify' no re-use
    let res = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "dog.openrusty.org")
        .header("verify", "0")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert!(headers.get("x-conn-reuse").is_none());
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_diff_verify_host_no_reuse() {
    init();
    let client = reqwest::Client::new();

    let req = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "cat.openrusty.org")
        .header("verify", "1")
        .header("verify_host", "1");

    // pre check re-use
    assert_reuse(req).await;

    // disable 'verify_host' no re-use
    let res = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "cat.openrusty.org")
        .header("verify", "1")
        .header("verify_host", "0")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert!(headers.get("x-conn-reuse").is_none());
}

#[cfg(feature = "any_tls")]
#[tokio::test]
async fn test_tls_diff_alt_cnt_no_reuse() {
    init();
    let client = reqwest::Client::new();

    let req = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "openrusty.org")
        .header("alt", "cat.com")
        .header("verify", "1")
        .header("verify_host", "1");

    // pre check re-use
    assert_reuse(req).await;

    // use alt-cn no reuse
    let res = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "openrusty.org")
        .header("alt", "dog.com")
        .header("verify", "1")
        .header("verify_host", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert!(headers.get("x-conn-reuse").is_none());
}

#[cfg(feature = "s2n")]
#[tokio::test]
async fn test_tls_psk() {
    use crate::utils::server_utils::TEST_PSK_IDENTITY;

    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "openrusty.org")
        .header("psk_identity", TEST_PSK_IDENTITY)
        .header("x-port", "6151")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[cfg(feature = "s2n")]
#[tokio::test]
async fn test_tls_psk_invalid() {
    init();
    let client = reqwest::Client::new();

    let res = client
        .get("http://127.0.0.1:6149/")
        .header("sni", "openrusty.org")
        .header("psk_identity", "BAD_IDENTITY")
        .header("x-port", "6151")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_error_before_headers_sent() {
    init();
    let url = "http://127.0.0.1:6146/sleep/test_error_before_headers_sent.txt";

    let tcp = TcpStream::connect("127.0.0.1:6146").await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    tokio::spawn(async move {
        h2.await.unwrap();
    });

    let request = Request::builder()
        .uri(url)
        .header("x-set-sleep", "0")
        .header("x-abort", "true")
        .body(())
        .unwrap();

    let (response, mut _stream) = client.send_request(request, true).unwrap();

    let response = response.await.unwrap();
    let mut body = response.into_body();

    while let Some(chunk) = body.data().await {
        assert_eq!(chunk.unwrap(), Bytes::new());
    }
}

#[tokio::test]
async fn test_error_after_headers_sent_rst_received() {
    init();
    let url = "http://127.0.0.1:6146/connection_die/test_error_after_headers_sent_rst_received.txt";

    let tcp = TcpStream::connect("127.0.0.1:6146").await.unwrap();
    let (mut client, h2) = client::handshake(tcp).await.unwrap();

    tokio::spawn(async move {
        h2.await.unwrap();
    });

    let request = Request::builder().uri(url).body(()).unwrap();

    let (response, mut _stream) = client.send_request(request, true).unwrap();

    let response = response.await.unwrap();
    let mut body = response.into_body();

    match body.data().await.expect("response body frame or reset") {
        Ok(chunk) => {
            assert_eq!(chunk, Bytes::from_static(b"AAAAA"));

            let err = body
                .data()
                .await
                .expect("response body reset")
                .expect_err("expected stream reset");
            assert_eq!(err.reason().expect("reset reason"), h2::Reason::CANCEL);
        }
        Err(err) => {
            assert_eq!(err.reason().expect("reset reason"), h2::Reason::CANCEL);
        }
    }
}

#[tokio::test]
async fn test_103() {
    init();
    let res = reqwest::get("http://127.0.0.1:6147/103").await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers();
    assert_eq!(headers[header::CONTENT_LENGTH], "6");
    let body = res.text().await.unwrap();
    assert_eq!(body, "123456");
}

#[tokio::test]
async fn test_103_die() {
    init();
    let res = reqwest::get("http://127.0.0.1:6147/103-die").await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
}

// Push request body until the proxy stops granting send capacity, which means it has
// stopped draining the downstream body because its upstream write is parked.
//
// The first grant and the later ones need different deadlines. Before the first grant the
// proxy may still be accepting the connection and settling the H2 handshake, so a short
// deadline there measures startup rather than the behavior under test. Once data has
// flowed, a quiet period really does mean the write is parked.
//
// Returns the number of body bytes accepted by the downstream connection.
async fn flood_until_upstream_write_parks(req_body: &mut h2::SendStream<Bytes>) -> usize {
    use std::future::poll_fn;
    use std::time::Duration;

    const FIRST_GRANT_TIMEOUT: Duration = Duration::from_secs(10);
    const PARKED_TIMEOUT: Duration = Duration::from_millis(500);

    let mut sent = 0usize;
    while sent < 512 * 1024 {
        req_body.reserve_capacity(16 * 1024);
        let first_grant = sent == 0;
        let deadline = if first_grant {
            FIRST_GRANT_TIMEOUT
        } else {
            PARKED_TIMEOUT
        };
        let granted =
            match tokio::time::timeout(deadline, poll_fn(|cx| req_body.poll_capacity(cx))).await {
                Ok(Some(Ok(n))) => n,
                Ok(other) => panic!("downstream send capacity error: {other:?}"),
                Err(_) if first_grant => {
                    panic!("proxy never granted any request body capacity within {deadline:?}")
                }
                // no new capacity for a while: the proxy is parked on the upstream write
                Err(_) => break,
            };
        req_body
            .send_data(Bytes::from(vec![0u8; granted]), false)
            .unwrap();
        sent += granted;
    }
    sent
}

// A downstream RST_STREAM must be observed even while the proxy is blocked writing the
// request body to the upstream (parked on h2 flow control). Otherwise the stream is held
// open as a zombie: its handles stay referenced and the downstream connection-window
// credit is never released. On catching the RST the proxy should also cancel the
// upstream stream promptly.
#[tokio::test]
async fn test_h2_downstream_rst_while_upstream_write_blocked() {
    use std::future::poll_fn;
    use std::time::Duration;

    init();

    // An h2c upstream that accepts one request but never reads its body and never
    // sends window updates, so the proxy's upstream write blocks on flow control.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (reset_tx, reset_rx) = tokio::sync::oneshot::channel::<h2::Reason>();

    tokio::spawn(async move {
        let (io, _) = listener.accept().await.unwrap();
        let mut conn = h2::server::Builder::new()
            // tiny stream window so the proxy's write parks quickly
            .initial_window_size(1024)
            .handshake::<_, Bytes>(io)
            .await
            .unwrap();
        let (req, mut send_response) = conn.accept().await.unwrap().unwrap();
        // hold the body reader without reading it: no window updates are granted
        let _body = req.into_body();

        // keep driving the connection in the background
        tokio::spawn(async move {
            while let Some(res) = conn.accept().await {
                if res.is_err() {
                    break;
                }
            }
        });

        // wait for the proxy to reset our stream
        let reason = poll_fn(|cx| send_response.poll_reset(cx)).await.unwrap();
        let _ = reset_tx.send(reason);
    });

    // h2c downstream client to the proxy
    let tcp = TcpStream::connect("127.0.0.1:6146").await.unwrap();
    let (mut client, conn) = client::handshake(tcp).await.unwrap();
    tokio::spawn(async move {
        // ignore errors: the proxy may tear the connection down after the RST
        let _ = conn.await;
    });

    let req = Request::builder()
        .method("POST")
        .uri("http://127.0.0.1:6146/")
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .body(())
        .unwrap();

    let (_response, mut req_body) = client.send_request(req, false).unwrap();

    let sent = flood_until_upstream_write_parks(&mut req_body).await;
    // we must have at least filled the upstream stream window for the write to park
    assert!(sent >= 1024, "only sent {sent} bytes");

    // reset the stream while the proxy is blocked writing upstream
    req_body.send_reset(h2::Reason::CANCEL);

    // the proxy should catch the RST promptly (not hang on the blocked write)
    // and cancel the upstream stream
    let reason = tokio::time::timeout(Duration::from_secs(5), reset_rx)
        .await
        .expect("proxy did not cancel the upstream stream after the downstream RST")
        .expect("upstream watcher task died before observing a reset");
    assert_eq!(reason, h2::Reason::CANCEL);
}

// Same as above, but with caching enabled and a cacheable upstream response mid-admission:
// the downstream RST caught during the blocked upstream write must be ignored (like
// downstream read errors are during caching) so the cache fill can complete.
#[tokio::test]
async fn test_h2_downstream_rst_during_cache_fill() {
    use std::future::poll_fn;
    use std::time::{Duration, Instant};

    init();

    let uri = "http://127.0.0.1:6154/test_h2_downstream_rst_during_cache_fill";

    // An h2c upstream that never reads the request body (so the proxy's upstream write
    // parks on flow control) but streams a cacheable response: first chunk right away,
    // final chunk + EOS only after the test signals that downstream sent its RST.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        let (io, _) = listener.accept().await.unwrap();
        let mut conn = h2::server::Builder::new()
            // tiny stream window so the proxy's request body write parks quickly
            .initial_window_size(1024)
            .handshake::<_, Bytes>(io)
            .await
            .unwrap();
        let (req, mut send_response) = conn.accept().await.unwrap().unwrap();
        // hold the body reader without reading it: no window updates are granted
        let body = req.into_body();

        // keep driving the connection in the background
        tokio::spawn(async move {
            while let Some(res) = conn.accept().await {
                if res.is_err() {
                    break;
                }
            }
        });

        let resp = http::Response::builder()
            .status(200)
            .header("cache-control", "max-age=30")
            .body(())
            .unwrap();
        let mut resp_body = send_response.send_response(resp, false).unwrap();
        resp_body
            .send_data(Bytes::from_static(b"hello "), false)
            .unwrap();

        // hold the end of the response so the cache fill is still in progress
        // when the downstream resets
        finish_rx.await.unwrap();
        resp_body
            .send_data(Bytes::from_static(b"world!"), true)
            .unwrap();

        // Keep the stream handles alive until the test is done. Dropping them here
        // would make h2 send an implicit RST_STREAM(NO_ERROR) (response complete
        // without consuming the request body), which the proxy's upstream read can
        // observe before the clean EOS and abort the cache admission.
        let _ = done_rx.await;
        drop(body);
        drop(resp_body);
    });

    // h2c downstream client to the caching proxy service
    let tcp = TcpStream::connect("127.0.0.1:6154").await.unwrap();
    let (mut client, conn) = client::handshake(tcp).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-h2", "true")
        .header("x-port", upstream_port.to_string())
        .body(())
        .unwrap();
    let (response, mut req_body) = client.send_request(req, false).unwrap();

    // a small first body chunk, fits the upstream window
    req_body.reserve_capacity(16);
    let granted = tokio::time::timeout(
        Duration::from_secs(10),
        poll_fn(|cx| req_body.poll_capacity(cx)),
    )
    .await
    .expect("proxy never granted the initial request body capacity")
    .unwrap()
    .unwrap();
    assert!(granted >= 16);
    req_body
        .send_data(Bytes::from_static(b"upload.........."), false)
        .unwrap();

    // wait for the response header and first chunk: the miss admission
    // (cache fill) is now in progress
    let (head, mut resp_body) = response.await.unwrap().into_parts();
    assert_eq!(head.status, 200);
    assert_eq!(head.headers.get("x-cache-status").unwrap(), "miss");
    let chunk = resp_body.data().await.unwrap().unwrap();
    assert_eq!(&chunk[..], b"hello ");
    let _ = resp_body.flow_control().release_capacity(chunk.len());

    // flood the request body until the proxy stops granting capacity, i.e. it is
    // parked writing to the upstream whose window is exhausted
    let sent = flood_until_upstream_write_parks(&mut req_body).await;
    assert!(sent >= 1024, "only sent {sent} bytes");

    // reset the stream while the proxy is blocked writing upstream;
    // since a cache fill is in progress, the proxy should swallow this error
    // and keep admitting the upstream response
    let warn_log_calls_before = downstream_cache_warn_log_calls();
    req_body.send_reset(h2::Reason::CANCEL);

    // Wait for the proxy to actually report the ignored downstream error before letting
    // the upstream finish. Sleeping instead would let a slow machine finish the response
    // first, which still passes but silently stops covering the case under test.
    let deadline = Instant::now() + Duration::from_secs(10);
    while downstream_cache_warn_log_calls() == warn_log_calls_before {
        assert!(
            Instant::now() < deadline,
            "proxy never reported a downstream error ignored during caching"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    finish_tx.send(()).unwrap();

    // The object must now be fully in cache: a new request is a hit with the complete
    // body and does not need the (single use) upstream. Poll for the admission to land
    // rather than sleeping a fixed amount; a miss would reach for the upstream that is no
    // longer accepting, so bound each attempt as well.
    let deadline = Instant::now() + Duration::from_secs(10);
    let body = loop {
        assert!(
            Instant::now() < deadline,
            "cache admission did not complete: second request never became a hit"
        );

        let tcp = TcpStream::connect("127.0.0.1:6154").await.unwrap();
        let (mut client, conn) = client::handshake(tcp).await.unwrap();
        let conn_task = tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("x-port", upstream_port.to_string())
            .body(())
            .unwrap();
        let (response, _) = client.send_request(req, true).unwrap();

        let attempt = tokio::time::timeout(Duration::from_secs(1), response).await;
        if let Ok(Ok(response)) = attempt {
            let (head, mut resp_body) = response.into_parts();
            if head.status == StatusCode::OK
                && head.headers.get("x-cache-status").map(|v| v.as_bytes()) == Some(b"hit")
            {
                let mut body = Vec::new();
                while let Some(chunk) = resp_body.data().await {
                    let chunk = chunk.unwrap();
                    let _ = resp_body.flow_control().release_capacity(chunk.len());
                    body.extend_from_slice(&chunk);
                }
                break body;
            }
        }

        conn_task.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(body, b"hello world!");

    // release the upstream's stream handles
    let _ = done_tx.send(());
}

// The H2 upstream error path resets the upstream request body stream as soon as the
// downstream stream goes away. That is only safe because h2 keeps a stream's queued
// initial HEADERS when the stream has not been opened yet.
//
// A stream stays unopened whenever its HEADERS have not been flushed, most durably when
// the peer's SETTINGS_MAX_CONCURRENT_STREAMS is already exhausted. If a reset discarded
// those queued HEADERS, RST_STREAM would become the first frame of an idle stream, which
// RFC 9113 section 6.4 requires the peer to treat as a connection-level PROTOCOL_ERROR.
// The peer then tears down the whole connection, failing every other request multiplexed
// onto it, not just the one being reset.
//
// Assert the ordering guarantee directly against the h2 dependency: reset a stream before
// the connection task has ever been polled, so the HEADERS are guaranteed to still be
// queued, then check what actually reaches the peer first.
#[tokio::test]
async fn test_h2_reset_before_headers_flush_keeps_headers_first() {
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    const FRAME_TYPE_HEADERS: u8 = 0x1;
    const FRAME_TYPE_RST_STREAM: u8 = 0x3;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Minimal H2 peer: it only needs to read frames, so parse the 9 byte frame headers and
    // skip the payloads rather than pulling in a full server implementation.
    let peer = tokio::spawn(async move {
        let (mut io, _) = listener.accept().await.unwrap();

        let mut preface = [0u8; 24];
        io.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface[..], b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");

        // report the first frame that belongs to the request stream, ignoring
        // connection level frames such as SETTINGS and WINDOW_UPDATE
        loop {
            let mut header = [0u8; 9];
            io.read_exact(&mut header).await.unwrap();
            let len = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
            let frame_type = header[3];
            let stream_id =
                u32::from_be_bytes([header[5], header[6], header[7], header[8]]) & 0x7fff_ffff;

            let mut payload = vec![0u8; len];
            io.read_exact(&mut payload).await.unwrap();

            if stream_id == 1 {
                return frame_type;
            }
        }
    });

    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    // handshake() only writes the client preface, it does not flush the queued frames,
    // so nothing below reaches the peer until the connection task is polled
    let (client, conn) = client::handshake(tcp).await.unwrap();
    let mut client = client.ready().await.unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("http://example.com/")
        .body(())
        .unwrap();
    let (_response, mut send_stream) = client.send_request(req, false).unwrap();

    // reset while the initial HEADERS are still queued on an unopened stream
    send_stream.send_reset(h2::Reason::CANCEL);

    let conn_task = tokio::spawn(async move {
        // the peer never replies, so the connection is expected to end in an error
        let _ = conn.await;
    });

    let first_frame = tokio::time::timeout(Duration::from_secs(5), peer)
        .await
        .expect("timed out waiting for the first frame on the request stream")
        .unwrap();

    assert_ne!(
        first_frame, FRAME_TYPE_RST_STREAM,
        "RST_STREAM was sent as the first frame of an unopened stream: peers reject this \
         with a connection level PROTOCOL_ERROR, which fails every stream on the connection"
    );
    assert_eq!(first_frame, FRAME_TYPE_HEADERS);

    drop(send_stream);
    let _ = conn_task.await;
}
