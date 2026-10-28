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

use pingora_test_utils::raw_http_origin::RawHttpOrigin;
use std::future::{pending, poll_fn, Future};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Barrier, Notify};

#[tokio::test]
async fn passes_raw_stream_and_peer_address() {
    let (peer_tx, peer_rx) = oneshot::channel();
    let peer_tx = Arc::new(std::sync::Mutex::new(Some(peer_tx)));
    let origin = RawHttpOrigin::bind({
        let peer_tx = Arc::clone(&peer_tx);
        move |stream, peer| {
            let peer_tx = Arc::clone(&peer_tx);
            async move {
                stream.readable().await.unwrap();
                let mut request = [0; 64];
                let len = stream.try_read(&mut request).unwrap();
                peer_tx
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send((peer, request[..len].to_vec()))
                    .unwrap();
            }
        }
    })
    .await
    .unwrap();

    assert!(origin.addr().ip().is_loopback());
    assert_ne!(origin.addr().port(), 0);
    assert_eq!(origin.url(), format!("http://{}", origin.addr()));

    let client = TcpStream::connect(origin.addr()).await.unwrap();
    let peer = client.local_addr().unwrap();
    client.writable().await.unwrap();
    client.try_write(b"raw request").unwrap();

    let (handler_peer, request) = peer_rx.await.unwrap();
    assert_eq!(handler_peer, peer);
    assert_eq!(request, b"raw request");
    origin.shutdown().await;
}

#[tokio::test]
async fn handles_connections_concurrently() {
    let barrier = Arc::new(Barrier::new(3));
    let origin = RawHttpOrigin::bind({
        let barrier = Arc::clone(&barrier);
        move |_stream, _peer| {
            let barrier = Arc::clone(&barrier);
            async move {
                barrier.wait().await;
            }
        }
    })
    .await
    .unwrap();

    let first = TcpStream::connect(origin.addr());
    let second = TcpStream::connect(origin.addr());
    let (first, second) = tokio::join!(first, second);
    drop(first.unwrap());
    drop(second.unwrap());

    tokio::time::timeout(Duration::from_secs(1), barrier.wait())
        .await
        .unwrap();
    origin.shutdown().await;
}

#[tokio::test]
async fn explicit_shutdown_aborts_active_connections() {
    let started = Arc::new(Notify::new());
    let origin = RawHttpOrigin::bind({
        let started = Arc::clone(&started);
        move |_stream, _peer| {
            let started = Arc::clone(&started);
            async move {
                started.notify_one();
                pending::<()>().await;
            }
        }
    })
    .await
    .unwrap();

    let mut client = TcpStream::connect(origin.addr()).await.unwrap();
    started.notified().await;
    origin.shutdown().await;

    wait_for_close(&mut client).await;
}

#[tokio::test]
async fn drop_stops_the_listener() {
    let origin = RawHttpOrigin::bind(|_stream, _peer| async {})
        .await
        .unwrap();
    let addr = origin.addr();

    TcpStream::connect(addr).await.unwrap();
    drop(origin);

    wait_until_refused(addr).await;
}

#[tokio::test]
async fn cancelling_shutdown_stops_the_listener_and_active_connections() {
    let started = Arc::new(Notify::new());
    let origin = RawHttpOrigin::bind({
        let started = Arc::clone(&started);
        move |_stream, _peer| {
            let started = Arc::clone(&started);
            async move {
                started.notify_one();
                pending::<()>().await;
            }
        }
    })
    .await
    .unwrap();
    let addr = origin.addr();

    let mut client = TcpStream::connect(addr).await.unwrap();
    started.notified().await;

    let mut shutdown = Box::pin(origin.shutdown());
    poll_fn(|context| {
        assert!(shutdown.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(shutdown);

    wait_until_refused(addr).await;
    wait_for_close(&mut client).await;
}

#[tokio::test]
#[should_panic(expected = "handler panic")]
async fn shutdown_propagates_handler_panics() {
    let started = Arc::new(Notify::new());
    let origin = RawHttpOrigin::bind({
        let started = Arc::clone(&started);
        move |_stream, _peer| {
            let started = Arc::clone(&started);
            async move {
                started.notify_one();
                panic!("handler panic");
            }
        }
    })
    .await
    .unwrap();

    TcpStream::connect(origin.addr()).await.unwrap();
    started.notified().await;
    origin.shutdown().await;
}

#[tokio::test]
async fn bind_to_propagates_listener_failures() {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let error =
        match RawHttpOrigin::bind_to(listener.local_addr().unwrap(), |_stream, _peer| async {})
            .await
        {
            Ok(_) => panic!("binding an occupied address unexpectedly succeeded"),
            Err(error) => error,
        };

    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
}

async fn wait_for_close(stream: &mut TcpStream) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            stream.readable().await.unwrap();
            let mut buffer = [0; 1];
            match stream.try_read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => panic!("unexpected response data"),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => panic!("failed to read closed connection: {error}"),
            }
        }
    })
    .await
    .unwrap();
}

async fn wait_until_refused(addr: SocketAddr) {
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
