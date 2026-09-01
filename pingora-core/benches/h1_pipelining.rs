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

use bytes::BytesMut;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use pingora_core::protocols::http::v1::server::HttpSession;
use tokio::runtime::Runtime;
use tokio_test::io::Builder;

const BODYLESS_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: pingora.org\r\n\r\n";
const CONTENT_LENGTH_REQUEST: &[u8] =
    b"POST / HTTP/1.1\r\nHost: pingora.org\r\nContent-Length: 4\r\n\r\nbody";
const CHUNKED_REQUEST: &[u8] = b"POST / HTTP/1.1\r\nHost: pingora.org\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\nX-Test: one\r\n\r\n";

fn pipeline(requests: usize, request: &[u8]) -> BytesMut {
    let mut pipeline = BytesMut::with_capacity(requests * request.len());
    for _ in 0..requests {
        pipeline.extend_from_slice(request);
    }
    pipeline
}

async fn consume_pipeline(prefix: BytesMut, requests: usize) {
    let mock_io = Builder::new().build();
    let mut session = HttpSession::new(Box::new(mock_io));
    session.set_pipelining_enabled(true);
    session.set_pipelined_prefix(prefix);

    for request in 0..requests {
        session.read_request().await.unwrap();

        let reusable = session.reuse().await.unwrap().unwrap();
        let (stream, prefix) = reusable.into_parts();
        if request + 1 == requests {
            assert!(prefix.is_none());
            break;
        }

        session = HttpSession::new(stream);
        session.set_pipelining_enabled(true);
        session.set_pipelined_prefix(prefix.unwrap());
    }
}

fn bench_h1_pipelining(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let mut group = c.benchmark_group("h1_pipelining");

    for (kind, request) in [
        ("bodyless", BODYLESS_REQUEST),
        ("content_length", CONTENT_LENGTH_REQUEST),
        ("chunked", CHUNKED_REQUEST),
    ] {
        for requests in [1, 16, 64, 256, 1024] {
            group.throughput(Throughput::Elements(requests));
            group.bench_with_input(
                BenchmarkId::new(kind, requests),
                &requests,
                |b, &requests| {
                    b.to_async(&runtime).iter_batched(
                        || pipeline(requests as usize, request),
                        |prefix| consume_pipeline(prefix, requests as usize),
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }

    group.finish();

    c.bench_function("h1_single_request", |b| {
        b.to_async(&runtime).iter_batched(
            || {
                let mock_io = Builder::new().read(BODYLESS_REQUEST).build();
                HttpSession::new(Box::new(mock_io))
            },
            |mut session| async move {
                session.read_request().await.unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_h1_pipelining);
criterion_main!(benches);
