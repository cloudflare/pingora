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

//! Connection-pool microbenchmarks.
//!
//! Run with: `cargo bench -p pingora-pool --bench connection_pool`
//!
//! These benchmarks include both no-eviction scaling cases (each thread
//! reuses its own key) and an eviction-pressure case (more unique keys than
//! pool capacity) so that eviction code paths actually get exercised under
//! concurrency.
//!
//! The LRU-only benchmark includes the LRU module directly with
//! `#[path = "../src/lru.rs"]` because `Lru` is `pub(crate)` and not part of
//! the public `pingora-pool` API. The shard counts used here are imported
//! from that module so they cannot drift from the implementation.

#[allow(dead_code, unused_imports)]
#[path = "../src/lru.rs"]
mod lru;

use lru::{Lru, N_SHARDS as LRU_SHARDS};
use pingora_pool::{ConnectionMeta, ConnectionPool};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const POOL_SIZE: usize = 1024;
const ROUND_TRIP_ITERATIONS: usize = 1_000_000;
const EVICTION_ITERATIONS: usize = 100_000;
const THREADED_ITERATIONS: usize = 250_000;
const LRU_ITERATIONS: usize = 250_000;
const THREAD_COUNTS: [usize; 5] = [1, 2, 4, 8, 16];
/// For eviction-pressure benchmarks, each thread cycles through this many
/// distinct keys so that put() routinely overflows the pool capacity.
/// 2x the per-thread cap on master so master's thread-local LRU evicts on
/// roughly half of every thread's puts; on the sharded branch the same
/// workload pushes total unique keys far above the global cap, so eviction
/// fires constantly there too.
const EVICTION_KEYS_PER_THREAD: u64 = (POOL_SIZE as u64) * 2;

fn print_result(name: &str, elapsed: Duration, iterations: usize) {
    println!(
        "{name:<32} {elapsed:?} total, {:?} avg/op",
        elapsed / iterations as u32
    );
}

fn bench_round_trip() {
    let pool = ConnectionPool::new(POOL_SIZE);
    let meta = ConnectionMeta::new(1, 1);
    pool.put(&meta, 1usize);

    let before = Instant::now();
    for _ in 0..ROUND_TRIP_ITERATIONS {
        let conn = pool.get(&meta.key).unwrap();
        black_box(conn);
        pool.put(&meta, conn);
    }
    print_result(
        "single-thread get+put",
        before.elapsed(),
        ROUND_TRIP_ITERATIONS,
    );
}

fn bench_eviction_pressure() {
    let pool = ConnectionPool::new(POOL_SIZE);
    let before = Instant::now();

    for id in 0..EVICTION_ITERATIONS {
        let meta = ConnectionMeta::new(id as u64, id as _);
        pool.put(&meta, black_box(id));
    }

    print_result(
        "put with eviction pressure",
        before.elapsed(),
        EVICTION_ITERATIONS,
    );
}

/// Use one distinct pool key per worker.
///
/// This intentionally avoids claiming a specific pool-map shard layout: the
/// connection pool may use a concurrent map whose internal shard selection is
/// not part of this crate's API.
fn pool_distinct_keys(threads: usize) -> Vec<u64> {
    let keys: Vec<_> = (0..threads as u64).collect();
    assert_eq!(keys.len(), threads);
    keys
}

fn bench_pool_pattern(label: &str, keys: Vec<u64>) {
    let threads = keys.len();
    let pool = Arc::new(ConnectionPool::new(POOL_SIZE));
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::with_capacity(threads);

    for (worker, key) in keys.into_iter().enumerate() {
        let pool = pool.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let meta = ConnectionMeta::new(key, worker as _);
            pool.put(&meta, worker);
            barrier.wait();

            // Timing starts after barrier release so warmup setup doesn't
            // count against the measured loop.
            let before = Instant::now();
            for _ in 0..THREADED_ITERATIONS {
                let conn = pool.get(&meta.key).unwrap();
                black_box(conn);
                pool.put(&meta, conn);
            }
            before.elapsed()
        }));
    }

    let elapsed: Duration = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .sum();
    print_result(
        &format!("pool {label} {threads:>2} threads"),
        elapsed / threads as u32,
        THREADED_ITERATIONS,
    );
}

/// Concurrent benchmark that intentionally exceeds pool capacity so the LRU
/// eviction path runs while threads contend on the pool.
fn bench_pool_eviction_pressure(threads: usize) {
    let pool = Arc::new(ConnectionPool::new(POOL_SIZE));
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::with_capacity(threads);

    for worker in 0..threads {
        let pool = pool.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let base = (worker as u64) * EVICTION_KEYS_PER_THREAD;
            barrier.wait();
            let before = Instant::now();
            for i in 0..THREADED_ITERATIONS {
                let key = base + (i as u64 % EVICTION_KEYS_PER_THREAD);
                let meta = ConnectionMeta::new(key, key as _);
                pool.put(&meta, black_box(worker));
            }
            before.elapsed()
        }));
    }

    let elapsed: Duration = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .sum();
    print_result(
        &format!("pool eviction {threads:>2} threads"),
        elapsed / threads as u32,
        THREADED_ITERATIONS,
    );
}

fn bench_pool_scaling() {
    println!("ConnectionPool get+put scaling");
    for threads in THREAD_COUNTS {
        bench_pool_pattern("distinct-keys", pool_distinct_keys(threads));
    }

    println!("ConnectionPool eviction-pressure scaling");
    for threads in THREAD_COUNTS {
        bench_pool_eviction_pressure(threads);
    }
}

fn lru_shard(key: &i32) -> usize {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() as usize % LRU_SHARDS
}

fn keys_spread_across_shards(threads: usize) -> Vec<i32> {
    let mut keys = Vec::with_capacity(threads);
    let mut used = [false; LRU_SHARDS];
    let mut key = 0;
    while keys.len() < threads {
        let shard = lru_shard(&key);
        if !used[shard] {
            used[shard] = true;
            keys.push(key);
        }
        key += 1;
    }
    assert_eq!(keys.len(), threads);
    assert_eq!(used.iter().filter(|used| **used).count(), threads);
    keys
}

fn keys_on_one_shard(threads: usize) -> Vec<i32> {
    let mut keys = Vec::with_capacity(threads);
    let mut key = 0;
    while keys.len() < threads {
        if lru_shard(&key) == 0 {
            keys.push(key);
        }
        key += 1;
    }
    assert_eq!(keys.len(), threads);
    assert!(keys.iter().all(|key| lru_shard(key) == 0));
    keys
}

fn bench_lru_pattern(label: &str, keys: Vec<i32>) {
    let threads = keys.len();
    let lru = Arc::new(Lru::new(POOL_SIZE));
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::with_capacity(threads);

    for key in keys {
        let lru = lru.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            let before = Instant::now();
            for _ in 0..LRU_ITERATIONS {
                let (_, evicted) = lru.add(key, ());
                black_box(evicted);
                black_box(lru.pop(&key));
            }
            before.elapsed()
        }));
    }

    let elapsed: Duration = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .sum();
    print_result(
        &format!("lru {label} {threads:>2} threads"),
        elapsed / threads as u32,
        LRU_ITERATIONS,
    );
}

fn bench_lru_scaling() {
    println!("LRU put+pop scaling");
    for threads in THREAD_COUNTS {
        bench_lru_pattern("spread", keys_spread_across_shards(threads));
        bench_lru_pattern("1-shard", keys_on_one_shard(threads));
    }
}

fn main() {
    println!("pingora-pool benchmarks");
    bench_round_trip();
    bench_eviction_pressure();
    bench_pool_scaling();
    bench_lru_scaling();
}
