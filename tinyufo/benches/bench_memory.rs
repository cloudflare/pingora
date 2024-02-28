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

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use rand::prelude::*;
use std::num::NonZeroUsize;

const ITERATIONS: usize = 5_000_000;

fn bench_lru(zip_exp: f64, items: usize, cache_size_percent: f32) {
    let cache_size = (cache_size_percent * items as f32).round() as usize;
    let mut lru = lru::LruCache::<u64, ()>::new(NonZeroUsize::new(cache_size).unwrap());

    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(items, zip_exp).unwrap();

    for _ in 0..ITERATIONS {
        let key = zipf.sample(&mut rng) as u64;

        if lru.get(&key).is_none() {
            lru.push(key, ());
        }
    }
}

fn bench_moka(zip_exp: f64, items: usize, cache_size_percent: f32) {
    let cache_size = (cache_size_percent * items as f32).round() as usize;
    let moka = moka::sync::Cache::new(cache_size as u64);

    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(items, zip_exp).unwrap();

    for _ in 0..ITERATIONS {
        let key = zipf.sample(&mut rng) as u64;

        if moka.get(&key).is_none() {
            moka.insert(key, ());
        }
    }
}

fn bench_tinyufo(zip_exp: f64, items: usize, cache_size_percent: f32) {
    let cache_size = (cache_size_percent * items as f32).round() as usize;
    let tinyufo = tinyufo::TinyUfo::new(cache_size, (cache_size as f32 * 1.0) as usize);

    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(items, zip_exp).unwrap();

    for _ in 0..ITERATIONS {
        let key = zipf.sample(&mut rng) as u64;

        if tinyufo.get(&key).is_none() {
            tinyufo.put(key, (), 1);
        }
    }
}

/*
cargo bench --bench bench_memory

total items 1000, cache size 10%
lru
dhat: At t-gmax: 9,408 bytes in 106 blocks
moka
dhat: At t-gmax: 354,232 bytes in 1,581 blocks
TinyUFO
dhat: At t-gmax: 37,337 bytes in 351 blocks

total items 10000, cache size 10%
lru
dhat: At t-gmax: 128,512 bytes in 1,004 blocks
moka
dhat: At t-gmax: 535,320 bytes in 7,278 blocks
TinyUFO
dhat: At t-gmax: 236,053 bytes in 2,182 blocks

total items 100000, cache size 10%
lru
dhat: At t-gmax: 1,075,648 bytes in 10,004 blocks
moka
dhat: At t-gmax: 2,489,088 bytes in 62,374 blocks
TinyUFO
dhat: At t-gmax: 2,290,635 bytes in 20,467 blocks
*/

fn main() {
    for items in [1000, 10_000, 100_000] {
        println!("\ntotal items {items}, cache size 10%");
        {
            let _profiler = dhat::Profiler::new_heap();
            bench_lru(1.05, items, 0.1);
            println!("lru");
        }

        {
            let _profiler = dhat::Profiler::new_heap();
            bench_moka(1.05, items, 0.1);
            println!("\nmoka");
        }

        {
            let _profiler = dhat::Profiler::new_heap();
            bench_tinyufo(1.05, items, 0.1);
            println!("\nTinyUFO");
        }
    }
}
