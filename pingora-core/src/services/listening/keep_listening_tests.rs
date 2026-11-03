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

//! Drives listener shutdown directly, sending phases and the shutdown broadcast in the order
//! `Server` does. Sockets are bound up front and handed over through the fd table.

#![cfg(unix)]

use super::*;
use crate::server::{ExecutionPhase, Fds};
use std::os::fd::IntoRawFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const REPLY: &[u8] = b"ok";
const DEADLINE: Duration = Duration::from_secs(5);

struct Reply;

#[async_trait]
impl ServerApp for Reply {
    async fn process_new(
        self: &Arc<Self>,
        mut session: Stream,
        _shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        session.write_all(REPLY).await.ok();
        session.shutdown().await.ok();
        None
    }
}

/// Binds `count` listeners and returns their addresses with an fd table holding them.
fn bind(count: usize) -> (Vec<String>, ListenFds) {
    let mut fds = Fds::new();
    let addrs = (0..count)
        .map(|_| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            fds.add(addr.clone(), listener.into_raw_fd());
            addr
        })
        .collect();
    (addrs, Arc::new(parking_lot::Mutex::new(fds)))
}

async fn accepts(addr: &str) -> bool {
    let Ok(Ok(mut stream)) = timeout(DEADLINE, TcpStream::connect(addr)).await else {
        return false;
    };
    let mut reply = Vec::new();
    matches!(
        timeout(DEADLINE, stream.read_to_end(&mut reply)).await,
        Ok(Ok(_))
    ) && reply == REPLY
}

/// Refused, not merely unanswered: a stalled accept loop keeps its socket open.
async fn refuses(addr: &str) -> bool {
    matches!(
        timeout(DEADLINE, TcpStream::connect(addr)).await,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused
    )
}

async fn eventually_refuses(addr: &str) {
    timeout(DEADLINE, async {
        while !refuses(addr).await {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{addr} was never refused"));
}

struct Setup {
    keep_listening: bool,
    endpoints: usize,
    listeners_per_fd: usize,
    phase_capacity: usize,
    phases_before_start: Vec<ExecutionPhase>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            keep_listening: true,
            endpoints: 1,
            listeners_per_fd: 1,
            phase_capacity: 16,
            phases_before_start: Vec::new(),
        }
    }
}

struct Running {
    addrs: Vec<String>,
    /// A listener that does not opt in, sharing the shutdown broadcast.
    witness: String,
    shutdown: watch::Sender<bool>,
    /// `None` once the phase channel is closed.
    phases: Option<broadcast::Sender<ExecutionPhase>>,
    tasks: Vec<JoinHandle<()>>,
}

fn spawn(
    mut service: Service<Reply>,
    fds: ListenFds,
    shutdown: watch::Receiver<bool>,
    listeners_per_fd: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        service
            .start_service(Some(fds), shutdown, listeners_per_fd)
            .await;
    })
}

async fn start(setup: Setup) -> Running {
    let (addrs, fds) = bind(setup.endpoints);
    let (witness_addrs, witness_fds) = bind(1);
    let (phases, phase_rx) = broadcast::channel(setup.phase_capacity);
    let (shutdown, shutdown_rx) = watch::channel(false);

    let mut service = Service::new("kept".to_string(), Reply);
    for addr in &addrs {
        service.add_tcp(addr);
    }
    if setup.keep_listening {
        service.keep_listening_with(phase_rx);
    }
    let mut witness = Service::new("witness".to_string(), Reply);
    witness.add_tcp(&witness_addrs[0]);
    for phase in setup.phases_before_start {
        phases.send(phase).unwrap();
    }

    let tasks = vec![
        spawn(service, fds, shutdown_rx.clone(), setup.listeners_per_fd),
        spawn(witness, witness_fds, shutdown_rx, 1),
    ];
    for addr in addrs.iter().chain(&witness_addrs) {
        assert!(accepts(addr).await, "{addr} never started accepting");
    }
    Running {
        addrs,
        witness: witness_addrs[0].clone(),
        shutdown,
        phases: Some(phases),
        tasks,
    }
}

impl Running {
    fn send(&self, phase: ExecutionPhase) {
        if let Some(phases) = &self.phases {
            phases.send(phase).ok();
        }
    }

    /// Mirrors `Server`: the upgrade phases precede the shutdown broadcast.
    fn upgrade(&self) {
        for phase in [
            ExecutionPhase::GracefulUpgradeTransferringFds,
            ExecutionPhase::GracefulUpgradeCloseTimeout,
        ] {
            self.send(phase);
        }
        self.shutdown.send(true).unwrap();
    }

    /// Mirrors `Server`: the terminate phase follows the shutdown broadcast.
    fn terminate(&self) {
        self.shutdown.send(true).unwrap();
        self.send(ExecutionPhase::GracefulTerminate);
    }

    async fn assert_keeps_accepting(self) {
        eventually_refuses(&self.witness).await;
        for addr in &self.addrs {
            assert!(accepts(addr).await, "{addr} stopped accepting");
        }
        assert!(!self.tasks[0].is_finished());
        self.tasks[0].abort();
    }

    async fn assert_stopped(self) {
        for addr in &self.addrs {
            eventually_refuses(addr).await;
        }
        for task in self.tasks {
            timeout(DEADLINE, task)
                .await
                .expect("service kept running")
                .unwrap();
        }
    }
}

#[tokio::test]
async fn graceful_terminate_keeps_accepting() {
    let running = start(Setup::default()).await;
    running.terminate();
    running.assert_keeps_accepting().await;
}

#[tokio::test]
async fn graceful_upgrade_closes_listener() {
    let running = start(Setup::default()).await;
    running.upgrade();
    running.assert_stopped().await;
}

#[tokio::test]
async fn graceful_terminate_closes_listener_without_opt_in() {
    let running = start(Setup {
        keep_listening: false,
        ..Default::default()
    })
    .await;
    running.terminate();
    running.assert_stopped().await;
}

/// A service that waits on a dependency can start after the upgrade began.
#[tokio::test]
async fn upgrade_reported_before_start_closes_listener() {
    let running = start(Setup {
        phases_before_start: vec![ExecutionPhase::GracefulUpgradeTransferringFds],
        ..Default::default()
    })
    .await;
    running.shutdown.send(true).unwrap();
    running.assert_stopped().await;
}

#[tokio::test]
async fn lagged_phases_close_listener() {
    let running = start(Setup {
        phase_capacity: 1,
        ..Default::default()
    })
    .await;
    running.send(ExecutionPhase::Running);
    running.send(ExecutionPhase::Running);
    running.terminate();
    running.assert_stopped().await;
}

#[tokio::test]
async fn closed_phases_close_listener() {
    let mut running = start(Setup::default()).await;
    running.phases = None;
    running.shutdown.send(true).unwrap();
    running.assert_stopped().await;
}

const CONCURRENT: Setup = Setup {
    keep_listening: true,
    endpoints: 3,
    listeners_per_fd: 4,
    phase_capacity: 16,
    phases_before_start: Vec::new(),
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_listeners_keep_accepting_on_terminate() {
    let running = start(CONCURRENT).await;
    running.terminate();
    running.assert_keeps_accepting().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_listeners_close_on_upgrade() {
    let running = start(CONCURRENT).await;
    running.upgrade();
    running.assert_stopped().await;
}
