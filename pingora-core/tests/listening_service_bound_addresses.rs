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

#![cfg(unix)]

use async_trait::async_trait;
use pingora_core::apps::ServerApp;
use pingora_core::listeners::ListenerConfig;
use pingora_core::protocols::Stream;
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch, ShutdownWatch};
use pingora_core::services::listening::{BoundAddressWatch, Service as ListeningService};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

struct TaggedApp(u8);

#[async_trait]
impl ServerApp for TaggedApp {
    async fn process_new(
        self: &Arc<Self>,
        mut stream: Stream,
        _shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        stream.write_all(&[self.0]).await.unwrap();
        None
    }
}

struct TestShutdown(Arc<Notify>);

#[async_trait]
impl ShutdownSignalWatch for TestShutdown {
    async fn recv(&self) -> ShutdownSignal {
        self.0.notified().await;
        ShutdownSignal::FastShutdown
    }
}

struct RunningServer {
    shutdown: Arc<Notify>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RunningServer {
    fn start(mut server: Server) -> Self {
        let shutdown = Arc::new(Notify::new());
        let shutdown_watch = TestShutdown(shutdown.clone());
        let thread = std::thread::spawn(move || {
            server.bootstrap();
            server.run(RunArgs {
                shutdown_signal: Box::new(shutdown_watch),
            });
        });
        Self {
            shutdown,
            thread: Some(thread),
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

async fn bound_tcp_address(watch: &mut BoundAddressWatch) -> std::net::SocketAddr {
    let addresses = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        watch.wait_for(Option::is_some),
    )
    .await
    .expect("listener did not bind in time")
    .expect("listening service exited before binding")
    .clone()
    .unwrap();
    *addresses[0].as_inet().unwrap()
}

async fn assert_service(address: std::net::SocketAddr, expected: u8) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    let mut response = [0];
    stream.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [expected]);
}

#[test]
fn reports_os_assigned_address() {
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());
    let mut service = ListeningService::new("listener".to_string(), TaggedApp(b'1'));
    service.add_tcp("127.0.0.1:0");
    let mut bound_addresses = service.watch_bound_addresses();
    server.add_service(service);
    let _server = RunningServer::start(server);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let address = bound_tcp_address(&mut bound_addresses).await;
            assert_ne!(address.port(), 0);
            assert_service(address, b'1').await;
        });
}

#[test]
fn duplicate_port_zero_listeners_bind_distinct_sockets() {
    let mut server = Server::new_with_opt_and_conf(None, ServerConf::default());

    let mut first = ListeningService::new("first".to_string(), TaggedApp(b'1'));
    first.add_listener(ListenerConfig::tcp("127.0.0.1:0").fd_transfer_id("first"));
    let mut first_addresses = first.watch_bound_addresses();
    server.add_service(first);

    let mut second = ListeningService::new("second".to_string(), TaggedApp(b'2'));
    second.add_listener(ListenerConfig::tcp("127.0.0.1:0").fd_transfer_id("second"));
    let mut second_addresses = second.watch_bound_addresses();
    server.add_service(second);

    let _server = RunningServer::start(server);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let first = bound_tcp_address(&mut first_addresses).await;
            let second = bound_tcp_address(&mut second_addresses).await;
            assert_ne!(first, second);
            assert_service(first, b'1').await;
            assert_service(second, b'2').await;
        });
}
