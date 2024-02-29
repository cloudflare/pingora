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

use pingora_timeout::*;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tokio::time::timeout as tokio_timeout;

const LOOP_SIZE: u32 = 100000;

async fn bench_timeout() -> u32 {
    let mut n = 0;
    for _ in 0..LOOP_SIZE {
        let fut = async { 1 };
        let to = timeout(Duration::from_secs(1), fut);
        n += to.await.unwrap();
    }
    n
}

async fn bench_tokio_timeout() -> u32 {
    let mut n = 0;
    for _ in 0..LOOP_SIZE {
        let fut = async { 1 };
        let to = tokio_timeout(Duration::from_secs(1), fut);
        n += to.await.unwrap();
    }
    n
}

async fn bench_fast_timeout() -> u32 {
    let mut n = 0;
    for _ in 0..LOOP_SIZE {
        let fut = async { 1 };
        let to = fast_timeout::fast_timeout(Duration::from_secs(1), fut);
        n += to.await.unwrap();
    }
    n
}

fn bench_tokio_timer() {
    let mut list = Vec::with_capacity(LOOP_SIZE as usize);
    let before = Instant::now();
    for _ in 0..LOOP_SIZE {
        list.push(sleep(Duration::from_secs(1)));
    }
    let elapsed = before.elapsed();
    println!(
        "tokio timer create {:?} total, {:?} avg per iteration",
        elapsed,
        elapsed / LOOP_SIZE
    );

    let before = Instant::now();
    drop(list);
    let elapsed = before.elapsed();
    println!(
        "tokio timer drop {:?} total, {:?} avg per iteration",
        elapsed,
        elapsed / LOOP_SIZE
    );
}

async fn bench_multi_thread_tokio_timer(threads: usize) {
    let mut handlers = vec![];
    for _ in 0..threads {
        let handler = tokio::spawn(async {
            bench_tokio_timer();
        });
        handlers.push(handler);
    }
    for thread in handlers {
        thread.await.unwrap();
    }
}

use std::sync::Arc;

async fn bench_multi_thread_timer(threads: usize, tm: Arc<TimerManager>) {
    let mut handlers = vec![];
    for _ in 0..threads {
        let tm_ref = tm.clone();
        let handler = tokio::spawn(async move {
            bench_timer(&tm_ref);
        });
        handlers.push(handler);
    }
    for thread in handlers {
        thread.await.unwrap();
    }
}

use pingora_timeout::timer::TimerManager;

fn bench_timer(tm: &TimerManager) {
    let mut list = Vec::with_capacity(LOOP_SIZE as usize);
    let before = Instant::now();
    for _ in 0..LOOP_SIZE {
        list.push(tm.register_timer(Duration::from_secs(1)));
    }
    let elapsed = before.elapsed();
    println!(
        "pingora timer create {:?} total, {:?} avg per iteration",
        elapsed,
        elapsed / LOOP_SIZE
    );

    let before = Instant::now();
    drop(list);
    let elapsed = before.elapsed();
    println!(
        "pingora timer drop {:?} total, {:?} avg per iteration",
        elapsed,
        elapsed / LOOP_SIZE
    );
}

#[tokio::main(worker_threads = 4)]
async fn main() {
    let before = Instant::now();
    bench_timeout().await;
    let elapsed = before.elapsed();
    println!(
        "pingora timeout {:?} total, {:?} avg per iteration",
        elapsed,
        elapsed / LOOP_SIZE
    );

    let before = Instant::now();
    bench_fast_timeout().await;
    let elapsed = before.elapsed();
    println!(
        "pingora fast timeout {:?} total, {:?} avg per iteration",
        elapsed,
        elapsed / LOOP_SIZE
    );

    let before = Instant::now();
    bench_tokio_timeout().await;
    let elapsed = before.elapsed();
    println!(
        "tokio timeout {:?} total, {:?} avg per iteration",
        elapsed,
        elapsed / LOOP_SIZE
    );

    println!("===========================");

    let tm = pingora_timeout::timer::TimerManager::new();
    bench_timer(&tm);
    bench_tokio_timer();

    println!("===========================");

    let tm = Arc::new(tm);
    bench_multi_thread_timer(4, tm).await;
    bench_multi_thread_tokio_timer(4).await;
}
