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

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use pingora_test_utils::http_origin::HttpOrigin;
use std::future::{poll_fn, Future};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};

#[tokio::test]
async fn serves_buffered_requests_with_shared_state() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let origin = HttpOrigin::bind({
        let requests = Arc::clone(&requests);
        move |request: Request<Bytes>| {
            let requests = Arc::clone(&requests);
            async move {
                requests.lock().await.push((
                    request.method().clone(),
                    request.uri().clone(),
                    request.body().clone(),
                ));
                Response::builder()
                    .status(StatusCode::CREATED)
                    .header("x-origin", "rust")
                    .body(Bytes::from_static(b"response body"))
                    .unwrap()
            }
        }
    })
    .await
    .unwrap();

    assert!(origin.addr().ip().is_loopback());
    assert_ne!(origin.addr().port(), 0);

    let response = reqwest::Client::new()
        .post(format!("{}/resource?query=value", origin.url()))
        .body("request body")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-origin"], "rust");
    assert_eq!(response.bytes().await.unwrap(), "response body");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, http::Method::POST);
    assert_eq!(requests[0].1, "/resource?query=value");
    assert_eq!(requests[0].2, "request body");
}

#[tokio::test]
async fn explicit_shutdown_stops_the_listener() {
    let origin = HttpOrigin::bind(ok_handler).await.unwrap();
    let addr = origin.addr();

    TcpStream::connect(addr).await.unwrap();
    origin.shutdown().await;

    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn shutdown_aborts_active_requests() {
    let started = Arc::new(Notify::new());
    let blocked = Arc::new(Notify::new());
    let origin = HttpOrigin::bind({
        let started = Arc::clone(&started);
        let blocked = Arc::clone(&blocked);
        move |_request| {
            let started = Arc::clone(&started);
            let blocked = Arc::clone(&blocked);
            async move {
                started.notify_one();
                blocked.notified().await;
                Response::new(Bytes::new())
            }
        }
    })
    .await
    .unwrap();

    let request = tokio::spawn(reqwest::get(origin.url()));
    started.notified().await;
    origin.shutdown().await;

    assert!(request.await.unwrap().is_err());
}

#[tokio::test]
async fn drop_stops_the_listener() {
    let origin = HttpOrigin::bind(ok_handler).await.unwrap();
    let addr = origin.addr();

    TcpStream::connect(addr).await.unwrap();
    drop(origin);

    wait_until_refused(addr).await;
}

#[tokio::test]
async fn cancelling_shutdown_stops_the_listener_and_active_requests() {
    let started = Arc::new(Notify::new());
    let blocked = Arc::new(Notify::new());
    let origin = HttpOrigin::bind({
        let started = Arc::clone(&started);
        let blocked = Arc::clone(&blocked);
        move |_request| {
            let started = Arc::clone(&started);
            let blocked = Arc::clone(&blocked);
            async move {
                started.notify_one();
                blocked.notified().await;
                Response::new(Bytes::new())
            }
        }
    })
    .await
    .unwrap();
    let addr = origin.addr();

    let request = tokio::spawn(reqwest::get(origin.url()));
    started.notified().await;

    let mut shutdown = Box::pin(origin.shutdown());
    poll_fn(|context| {
        assert!(shutdown.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(shutdown);

    wait_until_refused(addr).await;
    assert!(request.await.unwrap().is_err());
}

#[tokio::test]
#[should_panic(expected = "handler panic")]
async fn shutdown_propagates_handler_panics() {
    let origin = HttpOrigin::bind(panic_handler).await.unwrap();

    assert!(reqwest::get(origin.url()).await.is_err());
    origin.shutdown().await;
}

async fn ok_handler(_request: Request<Bytes>) -> Response<Bytes> {
    Response::new(Bytes::from_static(b"ok"))
}

async fn panic_handler(_request: Request<Bytes>) -> Response<Bytes> {
    panic!("handler panic")
}

async fn wait_until_refused(addr: std::net::SocketAddr) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match TcpStream::connect(addr).await {
                Ok(stream) => drop(stream),
                Err(_) => break,
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
