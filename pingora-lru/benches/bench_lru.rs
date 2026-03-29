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

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use rand::distributions::WeightedIndex;
use rand::prelude::*;
use std::sync::Arc;
use std::thread;
use std::time::Instant; // used in iter_custom timing

const WEIGHTS: &[usize] = &[
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 100, 100, 100,
    100, 100, 100, 100, 100, 100, 100,
];

const THREADS: usize = 8;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type StdLru = parking_lot::Mutex<lru::LruCache<u64, ()>>;

fn build_std_lru() -> StdLru {
    let cache = parking_lot::Mutex::new(lru::LruCache::<u64, ()>::unbounded());
    for i in 0..WEIGHTS.len() {
        cache.lock().put(i as u64, ());
    }
    cache
}

fn build_pingora_lru() -> pingora_lru::Lru<(), 10> {
    let plru = pingora_lru::Lru::<(), 10>::with_capacity(1000, 100);
    for i in 0..WEIGHTS.len() {
        plru.admit(i as u64, (), 1);
    }
    plru
}

// ---------------------------------------------------------------------------
// Single-threaded
// ---------------------------------------------------------------------------

/// Measures the cost of a single cache access/promotion.
/// Uses a non-uniform (Zipf-like) key distribution matching the original
/// benchmark: 10 out of 100 keys are 100× more likely to be sampled.
fn bench_single_thread(c: &mut Criterion) {
    let std_lru = build_std_lru();
    let plru = build_pingora_lru();
    let dist = WeightedIndex::new(WEIGHTS).unwrap();
    let mut rng = thread_rng();

    let mut group = c.benchmark_group("lru/single_thread");
    group.throughput(Throughput::Elements(1));

    group.bench_function("std_lru_get", |b| {
        b.iter(|| {
            std_lru.lock().get(&black_box(dist.sample(&mut rng) as u64));
        });
    });

    group.bench_function("pingora_lru_promote", |b| {
        b.iter(|| {
            plru.promote(black_box(dist.sample(&mut rng) as u64));
        });
    });

    group.bench_function("pingora_lru_promote_top_10", |b| {
        b.iter(|| {
            plru.promote_top_n(black_box(dist.sample(&mut rng) as u64), 10);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Concurrent (8 threads)
// ---------------------------------------------------------------------------

/// Measures throughput under contention with THREADS concurrent workers.
///
/// `iter_custom` is used because Criterion does not natively manage threads.
/// We time the wall-clock duration of all threads completing `iters`
/// operations each, which closely mirrors the original manual benchmark.
fn bench_concurrent(c: &mut Criterion) {
    let mut group = c.benchmark_group("lru/concurrent");
    // Report throughput as total operations across all threads.
    group.throughput(Throughput::Elements(THREADS as u64));

    group.bench_function("std_lru_get", |b| {
        let lru = Arc::new(build_std_lru());
        b.iter_custom(|iters| {
            let mut handles = Vec::with_capacity(THREADS);
            let start = Instant::now();
            for _ in 0..THREADS {
                let lru = Arc::clone(&lru);
                handles.push(thread::spawn(move || {
                    let mut rng = thread_rng();
                    let dist = WeightedIndex::new(WEIGHTS).unwrap();
                    for _ in 0..iters {
                        lru.lock().get(&black_box(dist.sample(&mut rng) as u64));
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            start.elapsed()
        });
    });

    group.bench_function("pingora_lru_promote", |b| {
        let plru = Arc::new(build_pingora_lru());
        b.iter_custom(|iters| {
            let mut handles = Vec::with_capacity(THREADS);
            let start = Instant::now();
            for _ in 0..THREADS {
                let plru = Arc::clone(&plru);
                handles.push(thread::spawn(move || {
                    let mut rng = thread_rng();
                    let dist = WeightedIndex::new(WEIGHTS).unwrap();
                    for _ in 0..iters {
                        plru.promote(black_box(dist.sample(&mut rng) as u64));
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            start.elapsed()
        });
    });

    group.bench_function("pingora_lru_promote_top_10", |b| {
        let plru = Arc::new(build_pingora_lru());
        b.iter_custom(|iters| {
            let mut handles = Vec::with_capacity(THREADS);
            let start = Instant::now();
            for _ in 0..THREADS {
                let plru = Arc::clone(&plru);
                handles.push(thread::spawn(move || {
                    let mut rng = thread_rng();
                    let dist = WeightedIndex::new(WEIGHTS).unwrap();
                    for _ in 0..iters {
                        plru.promote_top_n(black_box(dist.sample(&mut rng) as u64), 10);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            start.elapsed()
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

criterion_group!(benches, bench_single_thread, bench_concurrent);
criterion_main!(benches);
