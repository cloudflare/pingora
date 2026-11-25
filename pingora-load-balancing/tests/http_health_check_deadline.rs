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

use async_trait::async_trait;
use pingora_core::connectors::http::Connector;
use pingora_error::ErrorType::{Custom, ReadTimedout};
use pingora_load_balancing::discovery::Static;
use pingora_load_balancing::health_check::{HealthCheck, HealthObserve, HttpHealthCheck};
use pingora_load_balancing::{Backend, Backends};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};

const BOUND: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

struct Origin {
    backend: Backend,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    // Zero sends chunks indefinitely. Positive values terminate the next response.
    chunks: Arc<AtomicUsize>,
    closed: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl Origin {
    async fn new(chunks: usize, interval: Duration) -> Self {
        Self::on(
            TcpListener::bind("127.0.0.1:0").await.unwrap(),
            chunks,
            interval,
        )
        .await
    }

    async fn on(listener: TcpListener, chunks: usize, interval: Duration) -> Self {
        let backend = Backend::new(&listener.local_addr().unwrap().to_string()).unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let chunks = Arc::new(AtomicUsize::new(chunks));
        let (closed_tx, closed) = mpsc::unbounded_channel();
        let task = tokio::spawn({
            let connections = connections.clone();
            let requests = requests.clone();
            let chunks = chunks.clone();
            async move {
                let mut sessions = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (io, _) = accepted.unwrap();
                            connections.fetch_add(1, Relaxed);
                            let requests = requests.clone();
                            let chunks = chunks.clone();
                            let closed = closed_tx.clone();
                            sessions.spawn(async move {
                                let _ = serve(io, requests, chunks, interval).await;
                                let _ = closed.send(());
                            });
                        }
                        _ = sessions.join_next(), if !sessions.is_empty() => {}
                    }
                }
            }
        });
        Self {
            backend,
            connections,
            requests,
            chunks,
            closed,
            task,
        }
    }

    async fn wait_closed(&mut self) {
        timeout(BOUND, self.closed.recv()).await.unwrap().unwrap();
    }

    async fn wait_request(&self) {
        timeout(BOUND, async {
            while self.requests.load(Relaxed) == 0 {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut io: TcpStream,
    requests: Arc<AtomicUsize>,
    chunks: Arc<AtomicUsize>,
    interval: Duration,
) -> std::io::Result<()> {
    loop {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(io.read_u8().await?);
            assert!(head.len() < 4096);
        }
        requests.fetch_add(1, Relaxed);
        let count = chunks.load(Relaxed);
        io.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await?;
        let mut sent = 0;
        loop {
            io.write_all(b"2\r\nok\r\n").await?;
            sent += 1;
            sleep(interval).await;
            if sent == count {
                io.write_all(b"0\r\n\r\n").await?;
                break;
            }
        }
    }
}

fn short_check() -> HttpHealthCheck {
    let mut check = HttpHealthCheck::new("example.com", false);
    check.total_timeout = Some(PROBE_TIMEOUT);
    check
}

#[tokio::test]
async fn default_probe_bounds_an_endless_body() {
    let mut origin = Origin::new(0, Duration::from_millis(20)).await;
    let check = HttpHealthCheck::new("example.com", false);
    assert_eq!(check.total_timeout, Some(Duration::from_secs(3)));
    let custom = HttpHealthCheck::new_custom("example.com", false, Connector::new(None));
    assert_eq!(custom.total_timeout, check.total_timeout);
    let error = timeout(BOUND, check.check(&origin.backend))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.etype(), &Custom("health check timed out"));
    origin.wait_closed().await;
}

#[tokio::test]
async fn timed_out_connections_are_closed_and_not_reused() {
    for reuse in [false, true] {
        let mut origin = Origin::new(0, Duration::from_millis(20)).await;
        let mut check = short_check();
        check.reuse_connection = reuse;
        for _ in 0..2 {
            let error = timeout(BOUND, check.check(&origin.backend))
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.etype(), &Custom("health check timed out"));
            origin.wait_closed().await;
        }
        assert_eq!(origin.requests.load(Relaxed), 2);
        assert_eq!(origin.connections.load(Relaxed), 2);
    }
}

#[tokio::test]
async fn finite_bodies_are_drained_and_connections_can_be_reused() {
    for reuse in [false, true] {
        let origin = Origin::new(2, Duration::from_millis(20)).await;
        let mut check = short_check();
        check.reuse_connection = reuse;
        for _ in 0..2 {
            timeout(BOUND, check.check(&origin.backend))
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(origin.requests.load(Relaxed), 2);
        assert_eq!(origin.connections.load(Relaxed), if reuse { 1 } else { 2 });
    }
}

#[tokio::test]
async fn total_timeout_can_be_increased_or_disabled() {
    let origin = Origin::new(4, Duration::from_millis(100)).await;
    let mut check = short_check();
    let error = timeout(BOUND, check.check(&origin.backend))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.etype(), &Custom("health check timed out"));
    for budget in [None, Some(Duration::from_secs(2))] {
        check.total_timeout = budget;
        timeout(BOUND, check.check(&origin.backend))
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn per_read_timeout_still_applies() {
    let origin = Origin::new(2, Duration::from_millis(500)).await;
    let mut check = HttpHealthCheck::new("example.com", false);
    check.peer_template.options.read_timeout = Some(Duration::from_millis(50));
    let error = timeout(BOUND, check.check(&origin.backend))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.etype(), &ReadTimedout);
}

struct Observe(Arc<Mutex<Vec<bool>>>);

#[async_trait]
impl HealthObserve for Observe {
    async fn observe(&self, _target: &Backend, healthy: bool) {
        self.0.lock().unwrap().push(healthy);
    }
}

async fn pass_continues(parallel: bool) {
    let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (first, second) = if first.local_addr().unwrap() < second.local_addr().unwrap() {
        (first, second)
    } else {
        (second, first)
    };
    let stalled = Origin::on(first, 0, Duration::from_millis(20)).await;
    let good = Origin::on(second, 2, Duration::from_millis(20)).await;
    assert!(stalled.backend < good.backend);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut check = short_check();
    check.health_changed_callback = Some(Box::new(Observe(observations.clone())));
    let mut backends = Backends::new(Static::new(BTreeSet::from([
        stalled.backend.clone(),
        good.backend.clone(),
    ])));
    backends.set_health_check(Box::new(check));
    backends.update(|_| {}).await.unwrap();
    timeout(BOUND, backends.run_health_check(parallel))
        .await
        .unwrap();
    assert!(!backends.ready(&stalled.backend));
    assert!(backends.ready(&good.backend));
    assert_eq!(good.requests.load(Relaxed), 1);
    assert_eq!(*observations.lock().unwrap(), vec![false]);

    stalled.chunks.store(2, Relaxed);
    backends.update(|_| {}).await.unwrap();
    timeout(BOUND, backends.run_health_check(parallel))
        .await
        .unwrap();
    assert!(backends.ready(&stalled.backend));
    assert!(backends.ready(&good.backend));
    assert_eq!(stalled.requests.load(Relaxed), 2);
    assert_eq!(good.requests.load(Relaxed), 2);
    assert_eq!(*observations.lock().unwrap(), vec![false, true]);
}

#[tokio::test]
async fn serial_pass_continues_after_a_probe_times_out() {
    pass_continues(false).await;
}

#[tokio::test]
async fn parallel_pass_continues_after_a_probe_times_out() {
    pass_continues(true).await;
}

#[tokio::test]
async fn caller_cancellation_still_closes_the_connection() {
    let mut origin = Origin::new(0, Duration::from_millis(20)).await;
    let mut check = short_check();
    check.total_timeout = None;
    let mut probe = Box::pin(check.check(&origin.backend));
    tokio::select! {
        result = &mut probe => panic!("probe ended before cancellation: {result:?}"),
        _ = origin.wait_request() => {}
    }
    drop(probe);
    origin.wait_closed().await;
}
