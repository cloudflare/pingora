//! Criterion benchmarks for `AsyncLru` (lock-free read path, actor-based ordering).
//!
//! Run with: `cargo bench -p pingora-lru --bench bench_async_lru`

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use pingora_lru::async_lru::AsyncLru;
use rand::distributions::WeightedIndex;
use rand::prelude::*;
use std::sync::{Arc, Barrier};
use std::thread;

const SHARDS: usize = 32;
const ITEMS: usize = 100_000;

/// Build a weight distribution where the first `hot_count` items have `hot_weight`x access probability.
fn make_dist(n: usize, hot_count: usize, hot_weight: usize) -> WeightedIndex<usize> {
    let mut weights = vec![1usize; n];
    for w in weights.iter_mut().take(hot_count) {
        *w = hot_weight;
    }
    WeightedIndex::new(&weights).unwrap()
}

fn make_lru(items: usize) -> (AsyncLru<u64, SHARDS>, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let lru = {
        let _guard = rt.enter();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let cb = Arc::new(|_key: u64, _weight| async {});
        let lru =
            AsyncLru::<u64, SHARDS>::builder(items * 100, cb, shutdown_rx, rt.handle().clone())
                .capacity(items / SHARDS)
                .build();
        for i in 0..items {
            lru.admit(i as u64, 1);
        }
        // Let the actors process all the admits.
        std::thread::sleep(std::time::Duration::from_millis(100));
        lru
    };
    (lru, rt)
}

// ---------- peek (read-only, lock-free) ----------

fn bench_peek(c: &mut Criterion) {
    let mut group = c.benchmark_group("peek");
    let dist = Arc::new(make_dist(ITEMS, ITEMS / 10, 100));

    for threads in [1, 4, 8, 16, 32] {
        group.throughput(Throughput::Elements(1));

        group.bench_with_input(BenchmarkId::new("Lru", threads), &threads, |b, &threads| {
            let (lru, _rt) = make_lru(ITEMS);
            let lru = Arc::new(lru);
            b.iter_custom(|iters| {
                let barrier = Arc::new(Barrier::new(threads));
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        let lru = Arc::clone(&lru);
                        let dist = Arc::clone(&dist);
                        let barrier = Arc::clone(&barrier);
                        let per_thread = (iters as usize / threads).max(1000);
                        thread::spawn(move || {
                            let mut rng = thread_rng();
                            barrier.wait();
                            let start = std::time::Instant::now();
                            for _ in 0..per_thread {
                                let key = dist.sample(&mut rng) as u64;
                                std::hint::black_box(lru.peek(&key));
                            }
                            start.elapsed()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .max()
                    .unwrap()
            });
        });
    }
    group.finish();
}

// ---------- promote (write contention: channel send) ----------

fn bench_promote(c: &mut Criterion) {
    let mut group = c.benchmark_group("promote");
    let dist = Arc::new(make_dist(ITEMS, ITEMS / 10, 100));

    for threads in [1, 4, 8, 16, 32] {
        group.throughput(Throughput::Elements(1));

        group.bench_with_input(BenchmarkId::new("Lru", threads), &threads, |b, &threads| {
            let (lru, _rt) = make_lru(ITEMS);
            let lru = Arc::new(lru);
            b.iter_custom(|iters| {
                let barrier = Arc::new(Barrier::new(threads));
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        let lru = Arc::clone(&lru);
                        let dist = Arc::clone(&dist);
                        let barrier = Arc::clone(&barrier);
                        let per_thread = (iters as usize / threads).max(1000);
                        thread::spawn(move || {
                            let mut rng = thread_rng();
                            barrier.wait();
                            let start = std::time::Instant::now();
                            for _ in 0..per_thread {
                                let key = dist.sample(&mut rng) as u64;
                                lru.promote(&key);
                            }
                            start.elapsed()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .max()
                    .unwrap()
            });
        });
    }
    group.finish();
}

// ---------- mixed workload (90% peek, 10% promote) ----------

fn bench_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_90read_10write");
    let dist = Arc::new(make_dist(ITEMS, ITEMS / 10, 100));

    for threads in [1, 4, 8, 16, 32] {
        group.throughput(Throughput::Elements(1));

        group.bench_with_input(BenchmarkId::new("Lru", threads), &threads, |b, &threads| {
            let (lru, _rt) = make_lru(ITEMS);
            let lru = Arc::new(lru);
            b.iter_custom(|iters| {
                let barrier = Arc::new(Barrier::new(threads));
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        let lru = Arc::clone(&lru);
                        let dist = Arc::clone(&dist);
                        let barrier = Arc::clone(&barrier);
                        let per_thread = (iters as usize / threads).max(1000);
                        thread::spawn(move || {
                            let mut rng = thread_rng();
                            barrier.wait();
                            let start = std::time::Instant::now();
                            for _ in 0..per_thread {
                                let key = dist.sample(&mut rng) as u64;
                                if rng.gen_ratio(1, 10) {
                                    lru.promote(&key);
                                } else {
                                    std::hint::black_box(lru.peek(&key));
                                }
                            }
                            start.elapsed()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .max()
                    .unwrap()
            });
        });
    }
    group.finish();
}

// ---------- sustained throughput ----------

fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput_sustained");
    let dist = Arc::new(make_dist(ITEMS, ITEMS / 10, 100));

    for threads in [1, 4, 8, 16, 32] {
        group.throughput(Throughput::Elements(1));

        group.bench_with_input(BenchmarkId::new("Lru", threads), &threads, |b, &threads| {
            let (lru, _rt) = make_lru(ITEMS);
            let lru = Arc::new(lru);
            b.iter_custom(|iters| {
                let barrier = Arc::new(Barrier::new(threads));
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        let lru = Arc::clone(&lru);
                        let dist = Arc::clone(&dist);
                        let barrier = Arc::clone(&barrier);
                        let per_thread = (iters as usize / threads).max(1000);
                        thread::spawn(move || {
                            let mut rng = thread_rng();
                            barrier.wait();
                            let start = std::time::Instant::now();
                            for i in 0..per_thread {
                                let key = dist.sample(&mut rng) as u64;
                                match i % 10 {
                                    0 => {
                                        lru.admit(key, 1);
                                    }
                                    1..=4 => {
                                        std::hint::black_box(lru.peek(&key));
                                    }
                                    _ => {
                                        lru.promote(&key);
                                    }
                                }
                            }
                            start.elapsed()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .max()
                    .unwrap()
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_peek,
    bench_promote,
    bench_mixed,
    bench_throughput
);
criterion_main!(benches);
