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

//! Origins and a proxy launcher for the response header hold tests.

use bytes::Bytes;
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::{Fds, ListenFds};
use pingora_core::services::Service;
use pingora_proxy::ProxyHttp;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::fd::IntoRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Run `future` forever on a runtime of its own, so it outlives each test's runtime.
fn spawn_forever<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    });
}

fn bind_local() -> std::net::TcpListener {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

/// Serve `app` on an already bound local port, so the port cannot be taken
/// between choosing and binding it.
pub fn start_proxy<A>(app: A) -> SocketAddr
where
    A: ProxyHttp + Send + Sync + 'static,
    A::CTX: Send + Sync + 'static,
{
    let listener = bind_local();
    let addr = listener.local_addr().unwrap();
    let mut service = pingora_proxy::http_proxy_service(&Arc::new(ServerConf::default()), app);
    service.add_tcp(&addr.to_string());
    let fds: ListenFds = Arc::new(Fds::new().into());
    fds.lock().add(addr.to_string(), listener.into_raw_fd());
    spawn_forever(async move {
        let (_shutdown, watch) = watch::channel(false);
        service.start_service(Some(fds), watch, 1).await;
    });
    addr
}

/// One step of a scripted HTTP/1 origin response.
pub enum Step {
    Send(&'static [u8]),
    Pause(Duration),
}

/// An HTTP/1 origin that plays the script `route` returns for each request
/// path, then closes the connection.
pub struct H1Origin {
    pub addr: SocketAddr,
    connections: Arc<Mutex<HashMap<String, usize>>>,
}

impl H1Origin {
    pub fn start(route: fn(&str) -> Vec<Step>) -> Self {
        let listener = bind_local();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(Mutex::new(HashMap::new()));
        let counts = connections.clone();
        spawn_forever(async move {
            let listener = TcpListener::from_std(listener).unwrap();
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let counts = counts.clone();
                tokio::spawn(async move { serve_h1(sock, route, counts).await });
            }
        });
        Self { addr, connections }
    }

    /// How many connections requested `path`.
    pub fn connections(&self, path: &str) -> usize {
        *self.connections.lock().unwrap().get(path).unwrap_or(&0)
    }
}

async fn serve_h1(
    mut sock: TcpStream,
    route: fn(&str) -> Vec<Step>,
    counts: Arc<Mutex<HashMap<String, usize>>>,
) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let path = head.split(' ').nth(1).unwrap_or("/").to_string();
    *counts.lock().unwrap().entry(path.clone()).or_default() += 1;
    for step in route(&path) {
        match step {
            Step::Send(bytes) => {
                if sock.write_all(bytes).await.is_err() {
                    return;
                }
                sock.flush().await.ok();
            }
            Step::Pause(duration) => tokio::time::sleep(duration).await,
        }
    }
    sock.shutdown().await.ok();
}

/// An HTTP/2 origin that answers 200 with the body `route` returns, followed
/// by an `x-checksum` trailer when the path contains `/trailers`.
pub fn start_h2_origin(route: fn(&str) -> &'static [u8]) -> SocketAddr {
    let listener = bind_local();
    let addr = listener.local_addr().unwrap();
    spawn_forever(async move {
        let listener = TcpListener::from_std(listener).unwrap();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut connection = h2::server::handshake(socket).await.unwrap();
                while let Some(request) = connection.accept().await {
                    let (request, mut respond) = request.unwrap();
                    let path = request.uri().path();
                    let body = route(path);
                    let trailers = path.contains("/trailers");
                    let response = http::Response::builder()
                        .status(200)
                        .header(http::header::CONTENT_TYPE, "text/plain")
                        .header(http::header::CONTENT_LENGTH, body.len())
                        .body(())
                        .unwrap();
                    let mut stream = respond.send_response(response, false).unwrap();
                    stream
                        .send_data(Bytes::from_static(body), !trailers)
                        .unwrap();
                    if trailers {
                        let mut trailers = http::HeaderMap::new();
                        trailers.insert("x-checksum", "ok".parse().unwrap());
                        stream.send_trailers(trailers).unwrap();
                    }
                }
            });
        }
    });
    addr
}

/// Send a raw HTTP/1 request and read until the proxy closes the connection.
pub async fn raw_request(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("proxy did not close the connection")
        .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// A port with nothing listening on it.
pub fn closed_port() -> u16 {
    bind_local().local_addr().unwrap().port()
}
