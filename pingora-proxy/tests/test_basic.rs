// Copyright 2024 Cloudflare, Inc.
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

use hyper::{body::HttpBody, header::HeaderValue, Body, Client};
use hyperlocal::{UnixClientExt, Uri};
use reqwest::{header, StatusCode};

use utils::server_utils::init;

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
async fn test_h2_to_h1() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client.get("https://127.0.0.1:6150").send().await.unwrap();
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
async fn test_h2_to_h2() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .get("https://127.0.0.1:6150")
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

    let client = hyper::client::Client::builder()
        .http2_only(true)
        .build_http();

    let mut req = hyper::Request::builder()
        .uri("http://127.0.0.1:6146")
        .body(Body::empty())
        .unwrap();
    req.headers_mut()
        .insert("x-h2", HeaderValue::from_bytes(b"true").unwrap());
    let res = client.request(req).await.unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.version(), reqwest::Version::HTTP_2);

    let body = res.into_body().data().await.unwrap().unwrap();
    assert_eq!(body.as_ref(), b"Hello World!\n");
}

#[tokio::test]
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
async fn test_h2_to_h2_upload() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let payload = "test upload";

    let res = client
        .get("https://127.0.0.1:6150/echo")
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
async fn test_h2_to_h1_upload() {
    init();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let payload = "test upload";

    let res = client
        .get("https://127.0.0.1:6150/echo")
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
async fn test_simple_proxy_uds() {
    init();
    let url = Uri::new("/tmp/pingora_proxy.sock", "/").into();
    let client = Client::unix();

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

    let body = hyper::body::to_bytes(body).await.unwrap();
    assert_eq!(body.as_ref(), b"Hello World!\n");
}

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
    assert_eq!(headers["x-upstream-server-addr"], "/tmp/nginx-test.sock");

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

    let res = client
        .get("http://127.0.0.1:6147/bad_lb")
        .header("x-port", port)
        .send()
        .await
        .unwrap();

    // retry gives 200
    assert_eq!(res.status(), StatusCode::OK);
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

#[tokio::test]
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

async fn assert_reuse(req: reqwest::RequestBuilder) {
    req.try_clone().unwrap().send().await.unwrap();
    let res = req.send().await.unwrap();
    let headers = res.headers();
    assert!(headers.get("x-conn-reuse").is_some());
}

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
