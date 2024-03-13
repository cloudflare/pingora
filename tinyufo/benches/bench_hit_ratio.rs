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

const ITEMS: usize = 10_000;
const ITERATIONS: usize = 5_000_000;

fn bench_one(zip_exp: f64, cache_size_percent: f32) {
    print!("{zip_exp:.2}, {cache_size_percent:4}\t\t\t");
    let cache_size = (cache_size_percent * ITEMS as f32).round() as usize;
    let mut lru = lru::LruCache::<u64, ()>::new(NonZeroUsize::new(cache_size).unwrap());
    let moka = moka::sync::Cache::new(cache_size as u64);
    let quick_cache = quick_cache::sync::Cache::new(cache_size);
    let tinyufo = tinyufo::TinyUfo::new(cache_size, cache_size);

    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(ITEMS, zip_exp).unwrap();

    let mut lru_hit = 0;
    let mut moka_hit = 0;
    let mut quick_cache_hit = 0;
    let mut tinyufo_hit = 0;

    for _ in 0..ITERATIONS {
        let key = zipf.sample(&mut rng) as u64;

        if lru.get(&key).is_some() {
            lru_hit += 1;
        } else {
            lru.push(key, ());
        }

        if moka.get(&key).is_some() {
            moka_hit += 1;
        } else {
            moka.insert(key, ());
        }

        if quick_cache.get(&key).is_some() {
            quick_cache_hit += 1;
        } else {
            quick_cache.insert(key, ());
        }

        if tinyufo.get(&key).is_some() {
            tinyufo_hit += 1;
        } else {
            tinyufo.put(key, (), 1);
        }
    }

    print!("{:.2}%\t\t", lru_hit as f32 / ITERATIONS as f32 * 100.0);
    print!("{:.2}%\t\t", moka_hit as f32 / ITERATIONS as f32 * 100.0);
    print!(
        "{:.2}%\t\t",
        quick_cache_hit as f32 / ITERATIONS as f32 * 100.0
    );
    println!("{:.2}%", tinyufo_hit as f32 / ITERATIONS as f32 * 100.0);
}

/*
cargo bench --bench bench_hit_ratio

zipf & cache size               lru             moka            QuickC          TinyUFO
0.90, 0.005                     19.24%          33.43%          32.33%          33.35%
0.90, 0.01                      26.23%          37.86%          38.80%          40.06%
0.90, 0.05                      45.58%          55.13%          55.71%          57.80%
0.90,  0.1                      55.72%          64.15%          64.01%          66.36%
0.90, 0.25                      71.16%          77.12%          75.92%          78.53%
1.00, 0.005                     31.08%          45.68%          44.07%          45.15%
1.00, 0.01                      39.17%          50.80%          50.90%          52.30%
1.00, 0.05                      58.71%          66.92%          67.09%          68.79%
1.00,  0.1                      67.59%          74.28%          74.00%          75.92%
1.00, 0.25                      79.94%          84.35%          83.45%          85.28%
1.05, 0.005                     37.66%          51.78%          50.13%          51.12%
1.05, 0.01                      46.07%          57.13%          57.07%          58.41%
1.05, 0.05                      65.06%          72.37%          72.41%          73.93%
1.05,  0.1                      73.13%          78.97%          78.60%          80.24%
1.05, 0.25                      83.74%          87.41%          86.68%          88.14%
1.10, 0.005                     44.49%          57.84%          56.16%          57.28%
1.10, 0.01                      52.97%          63.19%          62.99%          64.24%
1.10, 0.05                      70.95%          77.24%          77.26%          78.55%
1.10,  0.1                      78.05%          82.86%          82.66%          84.01%
1.10, 0.25                      87.12%          90.10%          89.51%          90.66%
1.50, 0.005                     85.27%          89.92%          89.08%          89.69%
1.50, 0.01                      89.86%          92.77%          92.44%          92.94%
1.50, 0.05                      96.01%          97.08%          96.99%          97.23%
1.50,  0.1                      97.51%          98.15%          98.08%          98.24%
1.50, 0.25                      98.81%          99.09%          99.03%          99.09%
 */

fn main() {
    println!("zipf & cache size\t\tlru\t\tmoka\t\tQuickC\t\tTinyUFO",);
    for zif_exp in [0.9, 1.0, 1.05, 1.1, 1.5] {
        for cache_capacity in [0.005, 0.01, 0.05, 0.1, 0.25] {
            bench_one(zif_exp, cache_capacity);
        }
    }
}
