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
    let tinyufo = tinyufo::TinyUfo::new(cache_size, cache_size);
    let quick_cache = quick_cache::sync::Cache::new(cache_size);

    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(ITEMS, zip_exp).unwrap();

    let mut lru_hit = 0;
    let mut moka_hit = 0;
    let mut tinyufo_hit = 0;
    let mut quick_cache_hit = 0;

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

        if tinyufo.get(&key).is_some() {
            tinyufo_hit += 1;
        } else {
            tinyufo.put(key, (), 1);
        }

        if quick_cache.get(&key).is_some() {
            quick_cache_hit += 1;
        } else {
            quick_cache.insert(key, ());
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

zipf & cache size               lru             moka            QuickC.         TinyUFO
0.90, 0.005                     19.25%          33.48%          32.34%          33.37%
0.90, 0.01                      26.22%          37.81%          38.80%          40.13%
0.90, 0.05                      45.61%          55.24%          55.67%          57.82%
0.90,  0.1                      55.73%          64.21%          63.82%          66.35%
0.90, 0.25                      71.21%          77.17%          75.94%          78.55%
1.00, 0.005                     31.07%          45.65%          44.08%          45.17%
1.00, 0.01                      39.17%          50.75%          50.99%          52.28%
1.00, 0.05                      58.76%          67.01%          67.09%          68.86%
1.00,  0.1                      67.62%          74.44%          74.05%          75.96%
1.00, 0.25                      79.91%          84.37%          83.49%          85.28%
1.05, 0.005                     37.66%          51.82%          50.13%          51.18%
1.05, 0.01                      46.11%          57.11%          57.12%          58.42%
1.05, 0.05                      65.09%          72.38%          72.33%          73.95%
1.05,  0.1                      73.09%          78.91%          78.64%          80.19%
1.05, 0.25                      83.77%          87.44%          86.72%          88.16%
1.10, 0.005                     44.51%          57.89%          56.19%          57.36%
1.10, 0.01                      52.92%          63.15%          63.01%          64.22%
1.10, 0.05                      70.91%          77.20%          76.95%          78.52%
1.10,  0.1                      78.09%          83.00%          82.75%          84.02%
1.10, 0.25                      87.09%          90.08%          89.45%          90.62%
1.50, 0.005                     85.30%          89.92%          89.10%          89.70%
1.50, 0.01                      89.89%          92.77%          92.47%          92.95%
1.50, 0.05                      96.02%          97.09%          97.00%          97.24%
1.50,  0.1                      97.49%          98.14%          98.08%          98.23%
1.50, 0.25                      98.82%          99.10%          99.05%          99.10%
 */

fn main() {
    println!("zipf & cache size\t\tlru\t\tmoka\t\tQuickC.\t\tTinyUFO");
    for zif_exp in [0.9, 1.0, 1.05, 1.1, 1.5] {
        for cache_capacity in [0.005, 0.01, 0.05, 0.1, 0.25] {
            bench_one(zif_exp, cache_capacity);
        }
    }
}
