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

//! Benchmark for `Lru::promote()` vs `Lru::promote_top_n()`.
//!
//! Tests both small (original) and production-scale LRU sizes to show how
//! the `promote_top_n` optimization behaves at different scales.
//!
//! Run with: `cargo bench -p pingora-lru --bench bench_lru`
//!
//! ## Results (Apple M3 Max, 2026-04-03)
//!
//! Benchmark tiers simulate production-level data sizes (items/shard
//! ranging from ~100 to ~500K) with both uniform-hot and heavy-hitter
//! access patterns.
//!
//! ### 8-threaded — uniform hot set (10% of items are 100x hotter)
//!
//! | Items/shard | promote    | top_n(0) | top_n(3) | top_n(10) | top_n(50) | top_n(100) |
//! |-------------|------------|----------|----------|-----------|-----------|------------|
//! | 10 (orig)   | 366ns      | 476ns    | 271ns    | **164ns** | 164ns     | 164ns      |
//! | 100K (typ)  | **457ns**  | 480ns    | 437ns    | 520ns     | 1227ns    | 2394ns     |
//!
//! ### 8-threaded — heavy hitters (10 or 100 items are 10,000x hotter)
//!
//! | Items/shard     | promote    | top_n(0) | top_n(3) | top_n(10) | top_n(50) | top_n(100) |
//! |-----------------|------------|----------|----------|-----------|-----------|------------|
//! | 100K, 10 hot    | **649ns**  | 688ns    | 652ns    | 773ns     | 1811ns    | 3534ns     |
//! | 100K, 100 hot   | **607ns**  | 632ns    | 607ns    | 716ns     | 1493ns    | 2759ns     |
//!
//! ### Single-threaded — uniform hot set (10% of items are 100x hotter)
//!
//! | Items/shard | promote    | top_n(0) | top_n(3) | top_n(10) | top_n(50) | top_n(100) |
//! |-------------|------------|----------|----------|-----------|-----------|------------|
//! | 10 (orig)   | 22ns       | 20ns     | 29ns     | **30ns**  | 30ns      | 30ns       |
//! | 100K (typ)  | **297ns**  | 306ns    | 314ns    | 332ns     | 663ns     | 1092ns     |
//!
//! **Conclusions**:
//!
//! - `promote_top_n(0)` is strictly worse than `promote()` — it takes a
//!   wasted read lock before falling through to the write lock every time.
//!
//! - `promote_top_n(n)` for n > 0 only wins at the original small scale
//!   (10 items/shard) where the threshold covers the entire shard.
//!
//! - Even with heavy-hitter patterns (10 items at 10,000x weight),
//!   `promote()` ties or wins at production scale. With 10 hot items
//!   across 32 shards, most shards have 0-1 hot items, so the read-lock
//!   scan is wasted on the majority of cold-item accesses.
//!
//! - At production scale (~100K+ items/shard), plain `promote()` is fastest
//!   regardless of access pattern.

use rand::distributions::WeightedIndex;
use rand::prelude::*;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

const ITERATIONS: usize = 5_000_000;
const THREADS: usize = 8;

/// Build a weight distribution where the first `hot_count` items have
/// `hot_weight`x the access probability.
fn make_weights(n: usize, hot_count: usize, hot_weight: usize) -> Vec<usize> {
    let mut weights = vec![1usize; n];
    for w in weights.iter_mut().take(hot_count) {
        *w = hot_weight;
    }
    weights
}

fn bench_config(label: &str, items: usize, shards: usize, hot_pct: usize, hot_weight: usize) {
    let hot_count = items * hot_pct / 100;
    bench_config_abs(label, items, shards, hot_count, hot_weight);
}

fn bench_config_abs(label: &str, items: usize, shards: usize, hot_count: usize, hot_weight: usize) {
    println!(
        "\n=== {label}: {items} items, {shards} shards ({} per shard), \
         {hot_count} items are {hot_weight}x hotter ===",
        items / shards
    );

    let weights = make_weights(items, hot_count, hot_weight);
    let dist = Arc::new(WeightedIndex::new(&weights).unwrap());

    match shards {
        10 => bench_shards::<10>(items, &dist),
        32 => bench_shards::<32>(items, &dist),
        _ => panic!("unsupported shard count: {shards}"),
    }
}

/// Populate a fresh LRU with `items` entries.
fn make_lru<const N: usize>(items: usize) -> pingora_lru::Lru<(), N> {
    let lru = pingora_lru::Lru::<(), N>::with_capacity(items, items / N);
    for i in 0..items {
        lru.admit(i as u64, (), 1);
    }
    lru
}

fn bench_shards<const N: usize>(items: usize, dist: &Arc<WeightedIndex<usize>>) {
    // Each variant gets a fresh LRU to avoid state contamination from
    // prior runs warming hot items to the head.

    // --- Single-threaded ---
    println!("  Single-threaded:");
    {
        let lru = make_lru::<N>(items);
        let mut rng = thread_rng();
        let before = Instant::now();
        for _ in 0..ITERATIONS {
            lru.promote(dist.sample(&mut rng) as u64);
        }
        let elapsed = before.elapsed();
        println!(
            "    promote:        {elapsed:?} total, {:?} avg",
            elapsed / ITERATIONS as u32,
        );
    }

    for top_n in [0, 3, 10, 50, 100] {
        let lru = make_lru::<N>(items);
        let mut rng = thread_rng();
        let before = Instant::now();
        for _ in 0..ITERATIONS {
            lru.promote_top_n(dist.sample(&mut rng) as u64, top_n);
        }
        let elapsed = before.elapsed();
        println!(
            "    promote_top_{top_n:<3} {elapsed:?} total, {:?} avg",
            elapsed / ITERATIONS as u32,
        );
    }

    // --- Multi-threaded ---
    println!("  {THREADS}-threaded:");

    {
        let lru = Arc::new(make_lru::<N>(items));
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handlers = vec![];
        for _ in 0..THREADS {
            let lru = lru.clone();
            let dist = Arc::clone(dist);
            let barrier = barrier.clone();
            handlers.push(thread::spawn(move || {
                let mut rng = thread_rng();
                barrier.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    lru.promote(dist.sample(&mut rng) as u64);
                }
                before.elapsed()
            }));
        }
        let elapsed: Vec<_> = handlers.into_iter().map(|h| h.join().unwrap()).collect();
        let avg = elapsed.iter().sum::<std::time::Duration>() / THREADS as u32;
        println!(
            "    promote:        avg {avg:?}, {:?} avg per op",
            avg / ITERATIONS as u32,
        );
    }

    for top_n in [0, 3, 10, 50, 100] {
        let lru = Arc::new(make_lru::<N>(items));
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handlers = vec![];
        for _ in 0..THREADS {
            let lru = lru.clone();
            let dist = Arc::clone(dist);
            let barrier = barrier.clone();
            handlers.push(thread::spawn(move || {
                let mut rng = thread_rng();
                barrier.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    lru.promote_top_n(dist.sample(&mut rng) as u64, top_n);
                }
                before.elapsed()
            }));
        }
        let elapsed: Vec<_> = handlers.into_iter().map(|h| h.join().unwrap()).collect();
        let avg = elapsed.iter().sum::<std::time::Duration>() / THREADS as u32;
        println!(
            "    promote_top_{top_n:<3} avg {avg:?}, {:?} avg per op",
            avg / ITERATIONS as u32,
        );
    }
}

fn main() {
    // Benchmark tiers to simulate production-level data sizes:
    //   Small   = original bench scale (10 items/shard)
    //   Typical = ~100K items/shard (3.2M total across 32 shards)
    //   Large   = ~500K items/shard (16M total) — gated behind
    //             BENCH_LARGE=1 to avoid OOM on CI runners (~1.5GB heap)
    //
    // Note: the Typical tier allocates ~150MB per make_lru() call. With
    // multiple variants (promote + 5 top_n values) × configs, total peak
    // memory is ~1GB. Well within CI limits but notable for constrained machines.

    // Original benchmark scale (100 items, 10 shards = 10 per shard)
    bench_config("Small (original bench scale)", 100, 10, 10, 100);

    // Typical (~100K items/shard), 10% hot
    bench_config("Typical (100K/shard, 10% hot)", 3_200_000, 32, 10, 100);

    // Typical (~100K items/shard), heavy-hitter: only 10 items dominate
    // Simulates viral content / popular API endpoints where a handful of
    // assets receive the vast majority of traffic.
    bench_config_abs(
        "Typical (100K/shard, 10 heavy hitters)",
        3_200_000,
        32,
        10,
        10_000,
    );

    // Typical (~100K items/shard), moderate hot set: 100 items dominate
    bench_config_abs(
        "Typical (100K/shard, 100 heavy hitters)",
        3_200_000,
        32,
        100,
        10_000,
    );

    // Large (~500K items/shard, ~1.5GB heap)
    if std::env::var("BENCH_LARGE").is_ok() {
        bench_config("Large (500K/shard, 10% hot)", 16_000_000, 32, 10, 100);
    } else {
        println!("\n=== Skipping large bench (set BENCH_LARGE=1 to enable) ===");
    }
}
