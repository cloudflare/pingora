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

use rand::distributions::WeightedIndex;
use rand::prelude::*;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

// Non-uniform distributions, 100 items, 10 of them are 100x more likely to appear
const WEIGHTS: &[usize] = &[
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 100, 100, 100,
    100, 100, 100, 100, 100, 100, 100,
];

const ITERATIONS: usize = 5_000_000;
const THREADS: usize = 8;

fn main() {
    let lru = parking_lot::Mutex::new(lru::LruCache::<u64, ()>::unbounded());

    let plru = pingora_lru::Lru::<(), 10>::with_capacity(1000, 100);
    // populate first, then we bench access/promotion
    for i in 0..WEIGHTS.len() {
        lru.lock().put(i as u64, ());
    }
    for i in 0..WEIGHTS.len() {
        plru.admit(i as u64, (), 1);
    }

    // single thread
    let mut rng = thread_rng();
    let dist = WeightedIndex::new(WEIGHTS).unwrap();

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        lru.lock().get(&(dist.sample(&mut rng) as u64));
    }
    let elapsed = before.elapsed();
    println!(
        "lru promote total {elapsed:?}, {:?} avg per operation",
        elapsed / ITERATIONS as u32
    );

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        plru.promote(dist.sample(&mut rng) as u64);
    }
    let elapsed = before.elapsed();
    println!(
        "pingora lru promote total {elapsed:?}, {:?} avg per operation",
        elapsed / ITERATIONS as u32
    );

    let before = Instant::now();
    for _ in 0..ITERATIONS {
        plru.promote_top_n(dist.sample(&mut rng) as u64, 10);
    }
    let elapsed = before.elapsed();
    println!(
        "pingora lru promote_top_10 total {elapsed:?}, {:?} avg per operation",
        elapsed / ITERATIONS as u32
    );

    // concurrent

    let lru = Arc::new(lru);
    let mut handlers = vec![];
    for i in 0..THREADS {
        let lru = lru.clone();
        let handler = thread::spawn(move || {
            let mut rng = thread_rng();
            let dist = WeightedIndex::new(WEIGHTS).unwrap();
            let before = Instant::now();
            for _ in 0..ITERATIONS {
                lru.lock().get(&(dist.sample(&mut rng) as u64));
            }
            let elapsed = before.elapsed();
            println!(
                "lru promote total {elapsed:?}, {:?} avg per operation thread {i}",
                elapsed / ITERATIONS as u32
            );
        });
        handlers.push(handler);
    }
    for thread in handlers {
        thread.join().unwrap();
    }

    let plru = Arc::new(plru);

    let mut handlers = vec![];
    for i in 0..THREADS {
        let plru = plru.clone();
        let handler = thread::spawn(move || {
            let mut rng = thread_rng();
            let dist = WeightedIndex::new(WEIGHTS).unwrap();
            let before = Instant::now();
            for _ in 0..ITERATIONS {
                plru.promote(dist.sample(&mut rng) as u64);
            }
            let elapsed = before.elapsed();
            println!(
                "pingora lru promote total {elapsed:?}, {:?} avg per operation thread {i}",
                elapsed / ITERATIONS as u32
            );
        });
        handlers.push(handler);
    }
    for thread in handlers {
        thread.join().unwrap();
    }

    let mut handlers = vec![];
    for i in 0..THREADS {
        let plru = plru.clone();
        let handler = thread::spawn(move || {
            let mut rng = thread_rng();
            let dist = WeightedIndex::new(WEIGHTS).unwrap();
            let before = Instant::now();
            for _ in 0..ITERATIONS {
                plru.promote_top_n(dist.sample(&mut rng) as u64, 10);
            }
            let elapsed = before.elapsed();
            println!(
                "pingora lru promote_top_10 total {elapsed:?}, {:?} avg per operation thread {i}",
                elapsed / ITERATIONS as u32
            );
        });
        handlers.push(handler);
    }
    for thread in handlers {
        thread.join().unwrap();
    }
}
