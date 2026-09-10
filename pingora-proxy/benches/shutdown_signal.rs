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

//! Compares shutdown waiter registration and cancellation under contention.
//!
//! The benchmark intentionally omits request parsing and I/O shared by every
//! implementation so that synchronization overhead remains visible.

use futures::task::noop_waker;
use std::future::Future;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify};

const DEFAULT_ITERATIONS_PER_THREAD: usize = 20_000;
const DEFAULT_SAMPLES: usize = 5;
const MAX_SHUTDOWN_NOTIFY_SHARDS: usize = 256;

// These private types intentionally mirror the production sharding logic. Keep
// their shard cap, alignment, and thread-to-shard mapping in sync with src/lib.rs.
#[repr(align(128))]
struct NotifyShard(Notify);

struct ShardedNotify {
    shards: Box<[NotifyShard]>,
}

impl ShardedNotify {
    fn new(worker_threads: usize) -> Self {
        let shard_count = worker_threads
            .max(1)
            .checked_next_power_of_two()
            .unwrap_or(MAX_SHUTDOWN_NOTIFY_SHARDS)
            .min(MAX_SHUTDOWN_NOTIFY_SHARDS);
        Self {
            shards: (0..shard_count)
                .map(|_| NotifyShard(Notify::new()))
                .collect(),
        }
    }

    fn local(&self) -> &Notify {
        static NEXT_THREAD_ID: AtomicUsize = AtomicUsize::new(0);
        thread_local! {
            static THREAD_ID: usize = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
        }
        let thread_id = THREAD_ID.with(|thread_id| *thread_id);
        &self.shards[thread_id & (self.shards.len() - 1)].0
    }
}

fn poll_pending<F: Future>(mut future: Pin<&mut F>) {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
}

fn measure<F>(threads: usize, iterations: usize, operation: F) -> Duration
where
    F: Fn(usize) + Sync,
{
    let operation = &operation;
    thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(threads + 1));
        let handles: Vec<_> = (0..threads)
            .map(|thread_id| {
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..iterations {
                        operation(thread_id);
                    }
                })
            })
            .collect();

        barrier.wait();
        let started = Instant::now();
        for handle in handles {
            handle.join().unwrap();
        }
        started.elapsed()
    })
}

fn run_case<F>(name: &str, threads: usize, iterations: usize, samples: usize, operation: F)
where
    F: Fn(usize) + Sync,
{
    let mut elapsed: Vec<_> = (0..samples)
        .map(|_| measure(threads, iterations, &operation))
        .collect();
    elapsed.sort_unstable();
    let median = elapsed[elapsed.len() / 2];
    let operations = (threads * iterations) as f64;
    let operations_per_second = operations / median.as_secs_f64();
    let nanoseconds_per_operation = median.as_nanos() as f64 / operations;

    println!(
        "{name:<30} {threads:>3} {operations_per_second:>14.0} {nanoseconds_per_operation:>12.1}",
    );
}

fn configured_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn thread_counts(max_threads: usize) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut threads = 1;
    while threads < max_threads {
        counts.push(threads);
        threads *= 2;
    }
    counts.push(max_threads);
    counts.dedup();
    counts
}

fn main() {
    let available_threads = thread::available_parallelism().map_or(1, usize::from);
    let max_threads = configured_usize("PINGORA_BENCH_MAX_THREADS", available_threads);
    let iterations = configured_usize("PINGORA_BENCH_ITERATIONS", DEFAULT_ITERATIONS_PER_THREAD);
    let samples = configured_usize("PINGORA_BENCH_SAMPLES", DEFAULT_SAMPLES);

    println!("iterations per thread: {iterations}");
    println!("samples: {samples}");
    println!(
        "{:<30} {:>3} {:>14} {:>12}",
        "signal", "thr", "operations/s", "ns/op"
    );

    for threads in thread_counts(max_threads) {
        let notify = Notify::new();
        run_case("single Notify", threads, iterations, samples, |_| {
            let notified = notify.notified();
            let mut notified = pin!(notified);
            poll_pending(notified.as_mut());
        });

        let (watch_tx, watch_rx) = watch::channel(false);
        run_case("ShutdownWatch", threads, iterations, samples, |_| {
            let mut request_shutdown = watch_rx.clone();
            let changed = request_shutdown.wait_for(|shutdown| *shutdown);
            let mut changed = pin!(changed);
            poll_pending(changed.as_mut());
        });
        std::hint::black_box(&watch_tx);

        let sharded_notify = ShardedNotify::new(threads);
        let shutdown_flag = AtomicBool::new(false);
        run_case(
            "sharded Notify + poll flag",
            threads,
            iterations,
            samples,
            |_| {
                let notified = sharded_notify.local().notified();
                let mut notified = pin!(notified);
                poll_pending(notified.as_mut());
                std::hint::black_box(shutdown_flag.load(Ordering::Acquire));
            },
        );

        run_case(
            "sharded Notify + enable",
            threads,
            iterations,
            samples,
            |_| {
                let notified = sharded_notify.local().notified();
                let mut notified = pin!(notified);
                assert!(!notified.as_mut().enable());
                std::hint::black_box(shutdown_flag.load(Ordering::Acquire));
                poll_pending(notified.as_mut());
            },
        );
    }
}
