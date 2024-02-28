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

//! Pingora tokio runtime.
//!
//! Tokio runtime comes in two flavors: a single-threaded runtime
//! and a multi-threaded one which provides work stealing.
//! Benchmark shows that, compared to the single-threaded runtime, the multi-threaded one
//! has some overhead due to its more sophisticated work steal scheduling.
//!
//! This crate provides a third flavor: a multi-threaded runtime without work stealing.
//! This flavor is as efficient as the single-threaded runtime while allows the async
//! program to use multiple cores.

use pingora_runtime::{current_handle, Runtime};
use std::error::Error;
use std::{thread, time};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn hello_server(port: usize) -> Result<(), Box<dyn Error + Send>> {
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).await.unwrap();
    println!("Listening on: {}", addr);

    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        let rt = current_handle();
        rt.spawn(async move {
            loop {
                let mut buf = [0; 1024];
                let res = socket.read(&mut buf).await;

                let n = match res {
                    Ok(n) => n,
                    Err(_) => return,
                };

                if n == 0 {
                    return;
                }

                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\nconnection: keep-alive\r\n\r\nHello world!",
                    )
                    .await;
            }
        });
    }
}

/* On M1 macbook pro
wrk -t40 -c1000  -d10  http://127.0.0.1:3001  --latency
Running 10s test @ http://127.0.0.1:3001
  40 threads and 1000 connections
  Thread Stats   Avg      Stdev     Max   +/- Stdev
    Latency     3.53ms    0.87ms  17.12ms   84.99%
    Req/Sec     7.09k     1.29k   33.11k    93.30%
  Latency Distribution
     50%    3.56ms
     75%    3.95ms
     90%    4.37ms
     99%    5.38ms
  2844034 requests in 10.10s, 203.42MB read
Requests/sec: 281689.27
Transfer/sec:     20.15MB

wrk -t40 -c1000  -d10  http://127.0.0.1:3000  --latency
Running 10s test @ http://127.0.0.1:3000
  40 threads and 1000 connections
  Thread Stats   Avg      Stdev     Max   +/- Stdev
    Latency    12.16ms   16.29ms 112.29ms   83.40%
    Req/Sec     5.47k     2.01k   48.85k    83.67%
  Latency Distribution
     50%    2.09ms
     75%   20.23ms
     90%   37.11ms
     99%   65.16ms
  2190869 requests in 10.10s, 156.70MB read
Requests/sec: 216918.71
Transfer/sec:     15.52MB
*/

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rt = Runtime::new_steal(2, "");
    let handle = rt.get_handle();
    handle.spawn(hello_server(3000));
    let rt2 = Runtime::new_no_steal(2, "");
    let handle = rt2.get_handle();
    handle.spawn(hello_server(3001));
    thread::sleep(time::Duration::from_secs(999999999));
    Ok(())
}
