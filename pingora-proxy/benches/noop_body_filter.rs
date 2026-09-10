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

//! Per-call cost of `ProxyHttp::upstream_response_body_filter`.
//!
//! Run with: `cargo bench -p pingora-proxy --bench noop_body_filter`

use async_trait::async_trait;
use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pingora_core::protocols::l4::stream::Stream as L4Stream;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::Result;
use pingora_proxy::{ProxyHttp, Session};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

struct DefaultFilter;

#[async_trait]
impl ProxyHttp for DefaultFilter {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("not used by this benchmark")
    }
}

struct OverriddenFilter;

#[async_trait]
impl ProxyHttp for OverriddenFilter {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        unreachable!("not used by this benchmark")
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<Bytes>,
        _end_of_stream: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        Ok(None)
    }
}

async fn session() -> (Session, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (client, server) = tokio::join!(TcpStream::connect(addr), listener.accept());
    let mut client = client.unwrap();
    let (server, _) = server.unwrap();

    client
        .write_all(b"GET / HTTP/1.1\r\nhost: example.com\r\n\r\n")
        .await
        .unwrap();
    let mut session = Session::new_h1(Box::new(L4Stream::from(server)));
    session.read_request().await.unwrap();
    (session, client)
}

async fn call_filter<F: ProxyHttp<CTX = ()> + Send + Sync>(
    filter: &F,
    session: &mut Session,
    chunk: &Bytes,
) {
    let mut body = Some(chunk.clone());
    let mut ctx = ();
    black_box(
        filter
            .upstream_response_body_filter(
                black_box(session),
                black_box(&mut body),
                false,
                &mut ctx,
            )
            .unwrap(),
    );
}

fn benchmark(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut session, _client) = runtime.block_on(session());
    let chunk = Bytes::from(vec![0u8; 4096]);

    let mut group = c.benchmark_group("upstream_response_body_filter");
    group.bench_function("default", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let start = std::time::Instant::now();
                for _ in 0..iterations {
                    call_filter(&DefaultFilter, &mut session, &chunk).await;
                }
                start.elapsed()
            })
        })
    });
    group.bench_function("trivial_override", |b| {
        b.iter_custom(|iterations| {
            runtime.block_on(async {
                let start = std::time::Instant::now();
                for _ in 0..iterations {
                    call_filter(&OverriddenFilter, &mut session, &chunk).await;
                }
                start.elapsed()
            })
        })
    });
    group.finish();
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
