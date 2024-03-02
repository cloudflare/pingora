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

use rand::prelude::*;
use std::num::NonZeroUsize;
use std::sync::{Barrier, Mutex};
use std::thread;
use std::time::Instant;

const RO_ITEMS: usize = 5_000;
const RO_ITERATIONS: usize = 10_000_000;
const RW_CACHE_SIZE: usize = 5_000;
const RW_ITEMS: usize = 30_000;
const RW_ITERATIONS: usize = 5_000_000;
const ZIPF_EXP: f64 = 1.0;
const THREADS: usize = 8;

/*
cargo bench  --bench bench_perf

Note: the performance number vary a lot on different planform, CPU and CPU arch
Below is from Linux + i9-12900H CPU

lru read total 274.36625ms, 27ns avg per operation, 36447636 ops per second
moka read total 1.043477025s, 104ns avg per operation, 9583344 ops per second
quick_cache read total 232.407203ms, 23ns avg per operation, 43027928 ops per second
tinyufo read total 332.881911ms, 33ns avg per operation, 30040682 ops per second

lru read total 10.195350247s, 1.019µs avg per operation, 980839 ops per second
...
total 5580501 ops per second

moka read total 10.929958203s, 1.092µs avg per operation, 914916 ops per second
...
total 6746871 ops per second

quick_cache read total 1.382407295s, 138ns avg per operation, 7233758 ops per second
...
total 52143528 ops per second

tinyufo read total 540.87549ms, 54ns avg per operation, 18488544 ops per second
...
total 91168448 ops per second

lru mixed read/write 15.979100358s, 3.195µs avg per operation, 312908 ops per second, 23.01% misses
...
total 2403563 ops per second

moka mixed read/write 5.034107138s, 1.006µs avg per operation, 993224 ops per second 14.89% misses
...
total 7653049 ops per second

quick_cache mixed read/write 920.724286ms, 184ns avg per operation, 5430507 ops per second, 18.53% misses
...
total 38001608 ops per second

tinyufo mixed read/write 2.678294894s, 535ns avg per operation, 1866859 ops per second, 17.29% misses
...
total 11978863 ops per second
*/

fn main() {
    // we don't bench eviction here so make the caches large enough (10% extra) to hold all
    let lru = Mutex::new(lru::LruCache::<u64, ()>::unbounded());
    let moka = moka::sync::Cache::new((RO_ITEMS + RO_ITEMS / 10) as u64);
    let quick_cache = quick_cache::sync::Cache::new(RO_ITEMS + RO_ITEMS / 10);
    let tinyufo = tinyufo::TinyUfo::new(RO_ITEMS + RO_ITEMS / 10, RO_ITEMS + RO_ITEMS / 10);

    // populate first, then we bench access/promotion
    for i in 0..RO_ITEMS {
        lru.lock().unwrap().put(i as u64, ());
        moka.insert(i as u64, ());
        quick_cache.insert(i as u64, ());
        tinyufo.put(i as u64, (), 1);
    }

    // single thread
    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(RO_ITEMS, ZIPF_EXP).unwrap();

    let before = Instant::now();
    for _ in 0..RO_ITERATIONS {
        lru.lock().unwrap().get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "lru read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / RO_ITERATIONS as u32,
        (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    let before = Instant::now();
    for _ in 0..RO_ITERATIONS {
        moka.get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "moka read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / RO_ITERATIONS as u32,
        (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    let before = Instant::now();
    for _ in 0..RO_ITERATIONS {
        quick_cache.get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "quick_cache read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / RO_ITERATIONS as u32,
        (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    let before = Instant::now();
    for _ in 0..RO_ITERATIONS {
        tinyufo.get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "tinyufo read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / RO_ITERATIONS as u32,
        (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    // concurrent

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RO_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RO_ITERATIONS {
                    lru.lock().unwrap().get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "lru read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / RO_ITERATIONS as u32,
                    (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RO_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RO_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RO_ITERATIONS {
                    moka.get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "moka read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / RO_ITERATIONS as u32,
                    (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RO_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RO_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RO_ITERATIONS {
                    quick_cache.get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "quick_cache read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / RO_ITERATIONS as u32,
                    (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RO_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RO_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RO_ITERATIONS {
                    tinyufo.get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "tinyufo read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / RO_ITERATIONS as u32,
                    (RO_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RO_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    ///// bench mixed read and write /////

    let lru = Mutex::new(lru::LruCache::<u64, ()>::new(
        NonZeroUsize::new(RW_CACHE_SIZE).unwrap(),
    ));
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RW_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RW_ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    let mut lru = lru.lock().unwrap();
                    if lru.get(&key).is_none() {
                        lru.put(key, ());
                        miss_count += 1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "lru mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second, {:.2}% misses",
                    elapsed / RW_ITERATIONS as u32,
                    (RW_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32,
                    100f32 * miss_count as f32 / RW_ITERATIONS as f32,
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RW_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let moka = moka::sync::Cache::new(RW_CACHE_SIZE as u64);
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RW_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RW_ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    if moka.get(&key).is_none() {
                        moka.insert(key, ());
                        miss_count += 1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "moka mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second {:.2}% misses",
                    elapsed / RW_ITERATIONS as u32,
                    (RW_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32,
                    100f32 * miss_count as f32 / RW_ITERATIONS as f32,
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RW_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let quick_cache = quick_cache::sync::Cache::new(RW_CACHE_SIZE);
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RW_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RW_ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    if quick_cache.get(&key).is_none() {
                        quick_cache.insert(key, ());
                        miss_count +=1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "quick_cache mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second, {:.2}% misses",
                    elapsed / RW_ITERATIONS as u32,
                    (RW_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32,
                    100f32 * miss_count as f32 / RW_ITERATIONS as f32,
                );
            });
        }
    });

    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RW_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let tinyufo = tinyufo::TinyUfo::new(RW_CACHE_SIZE, RW_CACHE_SIZE);
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(RW_ITEMS, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..RW_ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    if tinyufo.get(&key).is_none() {
                        tinyufo.put(key, (), 1);
                        miss_count +=1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "tinyufo mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second, {:.2}% misses",
                    elapsed / RW_ITERATIONS as u32,
                    (RW_ITERATIONS as f32 / elapsed.as_secs_f32()) as u32,
                    100f32 * miss_count as f32 / RW_ITERATIONS as f32,
                );
            });
        }
    });

    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (RW_ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );
}
