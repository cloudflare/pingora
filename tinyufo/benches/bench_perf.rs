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

const ITEMS: usize = 100;

const ITERATIONS: usize = 5_000_000;
const THREADS: usize = 8;

/*
cargo bench  --bench bench_perf

Note: the performance number vary a lot on different planform, CPU and CPU arch
Below is from Linux + Ryzen 5 7600 CPU

lru read total 150.423567ms, 30ns avg per operation, 33239472 ops per second
moka read total 462.133322ms, 92ns avg per operation, 10819389 ops per second
quick_cache read total 125.618216ms, 25ns avg per operation, 39803144 ops per second
tinyufo read total 199.007359ms, 39ns avg per operation, 25124698 ops per second
tinyufo compact read total 331.145859ms, 66ns avg per operation, 15099087 ops per second

lru read total 5.402631847s, 1.08µs avg per operation, 925474 ops per second
...
total 6960329 ops per second

moka read total 2.742258211s, 548ns avg per operation, 1823314 ops per second
...
total 14072430 ops per second

quick_cache read total 1.186566627s, 237ns avg per operation, 4213838 ops per second
...
total 33694776 ops per second

tinyufo read total 208.346855ms, 41ns avg per operation, 23998444 ops per second
...
total 148691408 ops per second

tinyufo compact read total 539.403037ms, 107ns avg per operation, 9269507 ops per second
...
total 74130632 ops per second

lru mixed read/write 5.500309876s, 1.1µs avg per operation, 909039 ops per second, 407431 misses
...
total 6846743 ops per second

moka mixed read/write 2.368500882s, 473ns avg per operation, 2111040 ops per second 279324 misses
...
total 16557962 ops per second

quick_cache mixed read/write 838.072588ms, 167ns avg per operation, 5966070 ops per second 315051 misses
...
total 47698472 ops per second

tinyufo mixed read/write 456.134531ms, 91ns avg per operation, 10961678 ops per second, 294977 misses
...
total 80865792 ops per second

tinyufo compact mixed read/write 638.770053ms, 127ns avg per operation, 7827543 ops per second, 294641 misses
...
total 62600844 ops per second
*/

fn main() {
    println!("Note: these performance numbers vary a lot across different CPUs and OSes.");
    // we don't bench eviction here so make the caches large enough to hold all
    let lru = Mutex::new(lru::LruCache::<u64, ()>::unbounded());
    let moka = moka::sync::Cache::new(ITEMS as u64 + 10);
    let quick_cache = quick_cache::sync::Cache::new(ITEMS + 10);
    let tinyufo = tinyufo::TinyUfo::new(ITEMS + 10, 10);
    let tinyufo_compact = tinyufo::TinyUfo::new_compact(ITEMS + 10, 10);

    // populate first, then we bench access/promotion
    for i in 0..ITEMS {
        lru.lock().unwrap().put(i as u64, ());
        moka.insert(i as u64, ());
        quick_cache.insert(i as u64, ());
        tinyufo.put(i as u64, (), 1);
        tinyufo_compact.put(i as u64, (), 1);
    }

    // single thread
    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(ITEMS, 1.03).unwrap();

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        lru.lock().unwrap().get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "lru read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / ITERATIONS as u32,
        (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        moka.get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "moka read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / ITERATIONS as u32,
        (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        quick_cache.get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "quick_cache read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / ITERATIONS as u32,
        (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        tinyufo.get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "tinyufo read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / ITERATIONS as u32,
        (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        tinyufo_compact.get(&(zipf.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "tinyufo compact read total {elapsed:?}, {:?} avg per operation, {} ops per second",
        elapsed / ITERATIONS as u32,
        (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
    );

    // concurrent
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(ITEMS, 1.03).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    lru.lock().unwrap().get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "lru read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(ITEMS, 1.03).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    moka.get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "moka read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(ITEMS, 1.03).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    quick_cache.get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "quick_cache read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(ITEMS, 1.03).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    tinyufo.get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "tinyufo read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(ITEMS, 1.03).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    tinyufo_compact.get(&(zipf.sample(&mut rng) as u64));
                }
                let elapsed = before.elapsed();
                println!(
                    "tinyufo compact read total {elapsed:?}, {:?} avg per operation, {} ops per second",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    ///// bench mixed read and write /////
    const CACHE_SIZE: usize = 1000;
    let items: usize = 10000;
    const ZIPF_EXP: f64 = 1.3;

    let lru = Mutex::new(lru::LruCache::<u64, ()>::new(
        NonZeroUsize::new(CACHE_SIZE).unwrap(),
    ));
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(items, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    let mut lru = lru.lock().unwrap();
                    if lru.get(&key).is_none() {
                        lru.put(key, ());
                        miss_count += 1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "lru mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second, {miss_count} misses",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let moka = moka::sync::Cache::new(CACHE_SIZE as u64);
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(items, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    if moka.get(&key).is_none() {
                        moka.insert(key, ());
                        miss_count += 1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "moka mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second {miss_count} misses",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let quick_cache = quick_cache::sync::Cache::new(CACHE_SIZE);
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(items, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    if quick_cache.get(&key).is_none() {
                        quick_cache.insert(key, ());
                        miss_count += 1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "quick_cache mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second {miss_count} misses",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32
                );
            });
        }
    });
    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let tinyufo = tinyufo::TinyUfo::new(CACHE_SIZE, CACHE_SIZE);
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(items, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    if tinyufo.get(&key).is_none() {
                        tinyufo.put(key, (), 1);
                        miss_count +=1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "tinyufo mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second, {miss_count} misses",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32,
                );
            });
        }
    });

    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );

    let tinyufo_compact = tinyufo::TinyUfo::new(CACHE_SIZE, CACHE_SIZE);
    let wg = Barrier::new(THREADS);
    let before = Instant::now();
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                let mut miss_count = 0;
                let mut rng = thread_rng();
                let zipf = zipf::ZipfDistribution::new(items, ZIPF_EXP).unwrap();
                wg.wait();
                let before = Instant::now();
                for _ in 0..ITERATIONS {
                    let key = zipf.sample(&mut rng) as u64;
                    if tinyufo_compact.get(&key).is_none() {
                        tinyufo_compact.put(key, (), 1);
                        miss_count +=1;
                    }
                }
                let elapsed = before.elapsed();
                println!(
                    "tinyufo compact mixed read/write {elapsed:?}, {:?} avg per operation, {} ops per second, {miss_count} misses",
                    elapsed / ITERATIONS as u32,
                    (ITERATIONS as f32 / elapsed.as_secs_f32()) as u32,
                );
            });
        }
    });

    let elapsed = before.elapsed();
    println!(
        "total {} ops per second",
        (ITERATIONS as f32 * THREADS as f32 / elapsed.as_secs_f32()) as u32
    );
}
