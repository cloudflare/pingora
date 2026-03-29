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

use std::hint::black_box;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use pingora_lru::linked_list::LinkedList;

/// Number of items pre-loaded into the list for benchmarks that measure
/// a single operation against an existing list (promote, search, iter).
const BENCH_SIZE: usize = 1_000_000;

/// Smaller size used for bulk-pop benchmarks so each Criterion iteration
/// completes in a reasonable time while still being representative.
const POP_BATCH_SIZE: usize = 100_000;

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

/// Measures the cost of a single push onto a growing list.
/// Both lists start empty and grow without bound across iterations —
/// this is intentional: we want to capture steady-state push cost, not
/// amortised allocation cost.
fn bench_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("linked_list/push");
    group.throughput(Throughput::Elements(1));

    group.bench_function("std_push_front", |b| {
        let mut list = std::collections::LinkedList::<u64>::new();
        b.iter(|| {
            list.push_front(black_box(42u64));
        });
    });

    group.bench_function("pingora_push_head", |b| {
        let mut list = LinkedList::with_capacity(BENCH_SIZE);
        b.iter(|| {
            list.push_head(black_box(42u64));
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// iter
// ---------------------------------------------------------------------------

/// Measures a complete traversal of a pre-built list.
/// The list is constructed once and reused across all Criterion iterations.
fn bench_iter(c: &mut Criterion) {
    let mut std_list = std::collections::LinkedList::<u64>::new();
    let mut pingora_list = LinkedList::with_capacity(BENCH_SIZE);
    for i in 0..BENCH_SIZE as u64 {
        std_list.push_front(i);
        pingora_list.push_head(i);
    }

    let mut group = c.benchmark_group("linked_list/iter");
    group.throughput(Throughput::Elements(BENCH_SIZE as u64));

    group.bench_function("std_iter", |b| {
        b.iter(|| {
            // Fold to prevent the compiler from eliding the iteration.
            let sum: u64 = std_list.iter().fold(0u64, |acc, v| acc.wrapping_add(*v));
            black_box(sum);
        });
    });

    group.bench_function("pingora_iter", |b| {
        b.iter(|| {
            let sum: u64 = pingora_list
                .iter()
                .fold(0u64, |acc, v| acc.wrapping_add(*v));
            black_box(sum);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// search (first 10 elements)
// ---------------------------------------------------------------------------

/// Compares three ways to check whether a value exists within the first
/// 10 nodes of the list: std linear scan, pingora linear scan, and
/// pingora's optimised `exist_near_head` intrinsic.
fn bench_search(c: &mut Criterion) {
    let mut std_list = std::collections::LinkedList::<u64>::new();
    let mut pingora_list = LinkedList::with_capacity(BENCH_SIZE);
    // Value 1 is not near the head (head holds BENCH_SIZE-1), so every
    // search terminates after 10 comparisons — the worst case for the
    // "first 10" check.
    for i in 0..BENCH_SIZE as u64 {
        std_list.push_front(i);
        pingora_list.push_head(i);
    }

    let mut group = c.benchmark_group("linked_list/search_first_10");
    group.throughput(Throughput::Elements(1));

    group.bench_function("std_linear_search", |b| {
        b.iter(|| {
            black_box(std_list.iter().take(10).any(|v| *v == 1));
        });
    });

    group.bench_function("pingora_linear_search", |b| {
        b.iter(|| {
            black_box(pingora_list.iter().take(10).any(|v| *v == 1));
        });
    });

    group.bench_function("pingora_exist_near_head", |b| {
        b.iter(|| {
            black_box(pingora_list.exist_near_head(1, 10));
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// promote (move tail to head)
// ---------------------------------------------------------------------------

/// Measures a single tail→head move on a stable-sized list.
/// The std implementation has no dedicated promote: it does pop_back +
/// push_front. Pingora has a zero-copy `promote` that only rewires
/// pointers.
fn bench_promote(c: &mut Criterion) {
    let mut group = c.benchmark_group("linked_list/promote");
    group.throughput(Throughput::Elements(1));

    group.bench_function("std_pop_back_push_front", |b| {
        let mut list = std::collections::LinkedList::<u64>::new();
        for i in 0..BENCH_SIZE as u64 {
            list.push_front(i);
        }
        b.iter(|| {
            // Rotate: pop from back, push to front.
            let value = list.pop_back().unwrap();
            list.push_front(value);
        });
    });

    group.bench_function("pingora_promote", |b| {
        let mut list = LinkedList::with_capacity(BENCH_SIZE);
        for i in 0..BENCH_SIZE as u64 {
            list.push_head(i);
        }
        b.iter(|| {
            let index = list.tail().unwrap();
            list.promote(index);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// pop (drain the whole list)
// ---------------------------------------------------------------------------

/// Measures the throughput of draining an entire list.
/// `iter_batched` with `LargeInput` tells Criterion that setup is
/// expensive: it will build a fresh list per batch rather than per
/// individual iteration, and report time-per-batch.
fn bench_pop(c: &mut Criterion) {
    let mut group = c.benchmark_group("linked_list/pop");
    group.throughput(Throughput::Elements(POP_BATCH_SIZE as u64));

    group.bench_function("std_pop_back", |b| {
        b.iter_batched(
            || {
                let mut list = std::collections::LinkedList::<u64>::new();
                for i in 0..POP_BATCH_SIZE as u64 {
                    list.push_front(i);
                }
                list
            },
            |mut list| {
                for _ in 0..POP_BATCH_SIZE {
                    black_box(list.pop_back());
                }
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("pingora_pop_tail", |b| {
        b.iter_batched(
            || {
                let mut list = LinkedList::with_capacity(POP_BATCH_SIZE);
                for i in 0..POP_BATCH_SIZE as u64 {
                    list.push_head(i);
                }
                list
            },
            |mut list| {
                for _ in 0..POP_BATCH_SIZE {
                    black_box(list.pop_tail());
                }
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_push,
    bench_iter,
    bench_search,
    bench_promote,
    bench_pop,
);
criterion_main!(benches);
