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

//! Runs a real `Server` with an injected shutdown signal, so its own phase and shutdown ordering
//! decides each listener's fate. Sockets are bound up front and handed to the server's fd table.

#![cfg(all(unix, feature = "keep_listening_on_terminate"))]

use async_trait::async_trait;
use http::{Response, StatusCode};
use pingora_core::apps::http_app::{HttpServer, ServeHttp};
use pingora_core::apps::ServerApp;
use pingora_core::protocols::http::ServerSession;
use pingora_core::protocols::Stream;
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::{
    ExecutionPhase, ListenFds, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch, ShutdownWatch,
};
use pingora_core::services::background::{background_service, BackgroundService};
use pingora_core::services::listening::Service as ListeningService;
use pingora_core::services::Service;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::{IntoRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, Mutex};

const DEADLINE: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(1);

/// Delivers shutdown signals sent by the test instead of process signals.
struct InjectedSignal(Mutex<mpsc::UnboundedReceiver<ShutdownSignal>>);

#[async_trait]
impl ShutdownSignalWatch for InjectedSignal {
    async fn recv(&self) -> ShutdownSignal {
        self.0
            .lock()
            .await
            .recv()
            .await
            .unwrap_or(ShutdownSignal::FastShutdown)
    }
}

/// Adds its pre-bound socket to the server's fd table, so the listener reuses it and a graceful
/// upgrade transfers it.
struct PreBound<S> {
    inner: S,
    addr: String,
    fd: Option<RawFd>,
}

fn pre_bound<S>(inner: impl FnOnce(&str) -> S) -> PreBound<S> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    PreBound {
        inner: inner(&addr),
        addr,
        fd: Some(listener.into_raw_fd()),
    }
}

#[async_trait]
impl<S: Service> Service for PreBound<S> {
    async fn start_service(
        &mut self,
        fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
    ) {
        let fds = fds.expect("server provides an fd table");
        fds.lock().add(self.addr.clone(), self.fd.take().unwrap());
        self.inner
            .start_service(Some(fds), shutdown, listeners_per_fd)
            .await;
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn threads(&self) -> Option<usize> {
        Some(1)
    }
}

struct Reply;

#[async_trait]
impl ServerApp for Reply {
    async fn process_new(
        self: &Arc<Self>,
        mut session: Stream,
        _shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        session.write_all(b"ok").await.ok();
        session.shutdown().await.ok();
        None
    }
}

/// The supported readiness pattern: shutdown is observed separately from the listener.
struct Readiness {
    draining: Arc<AtomicBool>,
}

#[async_trait]
impl ServeHttp for Readiness {
    async fn response(&self, _session: &mut ServerSession) -> Response<Vec<u8>> {
        let (status, body) = if self.draining.load(Ordering::Relaxed) {
            (StatusCode::SERVICE_UNAVAILABLE, "draining\n")
        } else {
            (StatusCode::OK, "ready\n")
        };
        Response::builder()
            .status(status)
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(body.into())
            .unwrap()
    }
}

struct DrainWatch {
    draining: Arc<AtomicBool>,
}

#[async_trait]
impl BackgroundService for DrainWatch {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        shutdown.wait_for(|&shutting_down| shutting_down).await.ok();
        self.draining.store(true, Ordering::Relaxed);
    }
}

/// Returns the raw response to a fresh `GET /`, or `None` if nothing answers.
fn get(addr: &str) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&addr.parse().unwrap(), IO_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    (!response.is_empty()).then_some(response)
}

fn accepts(addr: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr.parse().unwrap(), IO_TIMEOUT) else {
        return false;
    };
    stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).is_ok() && reply == b"ok"
}

/// Refused, not merely unanswered: a stalled accept loop keeps its socket open.
fn refuses(addr: &str) -> bool {
    matches!(
        TcpStream::connect_timeout(&addr.parse().unwrap(), IO_TIMEOUT),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
    )
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < DEADLINE, "timed out waiting: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_phase(
    phases: &mut broadcast::Receiver<ExecutionPhase>,
    done: fn(&ExecutionPhase) -> bool,
) {
    eventually("execution phase", || loop {
        match phases.try_recv() {
            Ok(phase) if done(&phase) => return true,
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Empty) => return false,
            Err(e) => panic!("phase watch failed: {e}"),
        }
    });
}

/// Fails if runtime shutdown has begun, since it closes every listener regardless.
fn assert_before_runtime_shutdown(phases: &mut broadcast::Receiver<ExecutionPhase>) {
    loop {
        match phases.try_recv() {
            Ok(ExecutionPhase::ShutdownRuntimes | ExecutionPhase::Terminated) => {
                panic!("observed only after runtime shutdown began")
            }
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Empty) => return,
            Err(e) => panic!("phase watch failed: {e}"),
        }
    }
}

/// Leaves room to observe the drain before runtime shutdown.
const GRACE_PERIOD: Duration = Duration::from_secs(5);

struct Running {
    /// Opted in.
    kept: String,
    /// Not opted in.
    closed: String,
    phases: broadcast::Receiver<ExecutionPhase>,
    signal: mpsc::UnboundedSender<ShutdownSignal>,
    server: JoinHandle<()>,
}

/// Starts a server with the opted-in service `kept(&server, addr)` and a default listener.
fn run<S: Service + 'static>(
    conf: ServerConf,
    kept: impl FnOnce(&Server, &str) -> S,
    extra: impl FnOnce(&mut Server),
) -> Running {
    let mut server = Server::new_with_opt_and_conf(
        None,
        ServerConf {
            grace_period_seconds: Some(GRACE_PERIOD.as_secs()),
            graceful_shutdown_timeout_seconds: Some(1),
            ..conf
        },
    );
    let mut phases = server.watch_execution_phase();

    let kept = pre_bound(|addr| kept(&server, addr));
    let closed = pre_bound(|addr| {
        let mut service = ListeningService::new("closed".to_string(), Reply);
        service.add_tcp(addr);
        service
    });
    let (kept_addr, closed_addr) = (kept.addr.clone(), closed.addr.clone());
    server.add_service(kept);
    server.add_service(closed);
    extra(&mut server);

    let (signal, signal_rx) = mpsc::unbounded_channel();
    let server = thread::spawn(move || {
        server.bootstrap();
        server.run(RunArgs {
            shutdown_signal: Box::new(InjectedSignal(Mutex::new(signal_rx))),
        });
    });
    wait_for_phase(&mut phases, |p| matches!(p, ExecutionPhase::Running));
    eventually("default listener starts", || accepts(&closed_addr));
    Running {
        kept: kept_addr,
        closed: closed_addr,
        phases,
        signal,
        server,
    }
}

fn join(server: JoinHandle<()>) {
    eventually("server exits", || server.is_finished());
    server.join().unwrap();
}

#[test]
fn terminate_keeps_serving_readiness_until_runtime_shutdown() {
    let draining = Arc::new(AtomicBool::new(false));
    let watch = DrainWatch {
        draining: draining.clone(),
    };
    let Running {
        kept,
        closed,
        mut phases,
        signal,
        server,
    } = run(
        ServerConf::default(),
        |server, addr| {
            let mut service = ListeningService::new(
                "readiness".to_string(),
                HttpServer::new_app(Readiness { draining }),
            );
            service.add_tcp(addr);
            service.keep_listening_on_terminate(server);
            service
        },
        |server| {
            server.add_service(background_service("drain watch", watch));
        },
    );
    eventually("readiness ready", || {
        get(&kept).is_some_and(|r| r.starts_with("HTTP/1.1 200") && r.ends_with("ready\n"))
    });

    signal.send(ShutdownSignal::GracefulTerminate).unwrap();
    let draining_response = |r: &String| r.starts_with("HTTP/1.1 503") && r.ends_with("draining\n");
    eventually("readiness reports draining", || {
        get(&kept).is_some_and(|r| draining_response(&r))
    });
    for _ in 0..3 {
        let response = get(&kept).expect("readiness stopped answering during the drain");
        assert!(draining_response(&response), "{response}");
    }
    eventually("default listener refuses", || refuses(&closed));
    assert_before_runtime_shutdown(&mut phases);

    join(server);
    assert!(refuses(&kept), "readiness outlived runtime shutdown");
}

/// Socket transfer is Linux-only; elsewhere the server reports the upgrade without a taker.
#[cfg(target_os = "linux")]
fn take_over(upgrade_sock: String) -> JoinHandle<Vec<RawFd>> {
    thread::spawn(move || {
        let mut fds = pingora_core::server::Fds::new();
        fds.get_from_sock(upgrade_sock.as_str())
            .expect("socket transfer failed");
        fds.serialize().1
    })
}

#[test]
fn upgrade_hands_over_and_closes_kept_listener() {
    // Short and on a local filesystem: socket paths are length-limited, and the socket is chmodded.
    let upgrade_sock = std::env::temp_dir()
        .join(format!("keep_listening_{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&upgrade_sock);
    let mut running = run(
        ServerConf {
            upgrade_sock: upgrade_sock.clone(),
            ..Default::default()
        },
        |server, addr| {
            let mut service = ListeningService::new("kept".to_string(), Reply);
            service.add_tcp(addr);
            service.keep_listening_on_terminate(server);
            service
        },
        |_| {},
    );
    eventually("kept listener starts", || accepts(&running.kept));

    #[cfg(target_os = "linux")]
    let new_process = take_over(upgrade_sock);
    running
        .signal
        .send(ShutdownSignal::GracefulUpgrade)
        .unwrap();
    #[cfg(target_os = "linux")]
    {
        let transferred = new_process.join().unwrap();
        assert_eq!(transferred.len(), 2, "both listeners are handed over");
        // Closed so refused connections below prove the old listeners stopped.
        for fd in transferred {
            unsafe { libc::close(fd) };
        }
    }

    wait_for_phase(&mut running.phases, |p| {
        matches!(p, ExecutionPhase::ShutdownGracePeriod)
    });
    eventually("kept listener refuses", || refuses(&running.kept));
    eventually("default listener refuses", || refuses(&running.closed));
    assert_before_runtime_shutdown(&mut running.phases);
    join(running.server);
}
