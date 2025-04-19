// Copyright 2025 Cloudflare, Inc.
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
    let tinyufo_compact = tinyufo::TinyUfo::new_compact(cache_size, cache_size);

    let mut rng = rand::rng();
    let zipf = rand_distr::Zipf::new(ITEMS as f64, zip_exp).unwrap();

    let mut lru_hit = 0;
    let mut moka_hit = 0;
    let mut quick_cache_hit = 0;
    let mut tinyufo_hit = 0;
    let mut tinyufo_compact_hit = 0;

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

        if tinyufo_compact.get(&key).is_some() {
            tinyufo_compact_hit += 1;
        } else {
            tinyufo_compact.put(key, (), 1);
        }
    }

    print!("{:.2}%\t\t", lru_hit as f32 / ITERATIONS as f32 * 100.0);
    print!("{:.2}%\t\t", moka_hit as f32 / ITERATIONS as f32 * 100.0);
    print!(
        "{:.2}%\t\t",
        quick_cache_hit as f32 / ITERATIONS as f32 * 100.0
    );
    print!("{:.2}%\t\t", tinyufo_hit as f32 / ITERATIONS as f32 * 100.0);
    println!("{:.2}%", tinyufo_compact_hit as f32 / ITERATIONS as f32 * 100.0);
}

/*
cargo bench --bench bench_hit_ratio

zipf & cache size               lru             moka            QuickC          TinyUFO         TinyUFO Compact
0.90, 0.005                     19.12%          33.33%          32.77%          33.22%          32.25%
0.90, 0.01                      26.14%          37.98%          39.29%          40.00%          38.78%
0.90, 0.05                      45.55%          55.17%          56.48%          57.75%          56.52%
0.90,  0.1                      55.62%          64.14%          64.65%          66.28%          65.08%
0.90, 0.25                      71.15%          77.11%          76.69%          78.49%          77.45%
1.00, 0.005                     30.94%          45.57%          44.49%          45.08%          43.98%
1.00, 0.01                      39.02%          50.64%          51.40%          52.17%          51.20%
1.00, 0.05                      58.68%          66.88%          67.64%          68.77%          67.70%
1.00,  0.1                      67.60%          74.22%          74.67%          75.91%          75.04%
1.00, 0.25                      79.94%          84.35%          83.94%          85.28%          84.48%
1.05, 0.005                     37.53%          51.70%          50.56%          51.09%          50.17%
1.05, 0.01                      45.96%          56.99%          57.49%          58.33%          57.22%
1.05, 0.05                      64.97%          72.18%          72.89%          73.85%          72.95%
1.05,  0.1                      73.04%          78.91%          79.11%          80.16%          79.34%
1.05, 0.25                      83.72%          87.39%          87.10%          88.13%          87.40%
1.10, 0.005                     44.33%          57.76%          56.57%          57.15%          56.10%
1.10, 0.01                      52.85%          63.05%          63.33%          64.17%          63.19%
1.10, 0.05                      70.90%          77.25%          77.66%          78.52%          77.79%
1.10,  0.1                      78.07%          82.99%          83.14%          84.01%          83.25%
1.10, 0.25                      87.05%          90.05%          89.77%          90.59%          89.99%
1.50, 0.005                     85.20%          89.84%          89.26%          89.64%          89.26%
1.50, 0.01                      89.79%          92.74%          92.57%          92.89%          92.55%
1.50, 0.05                      96.01%          97.05%          97.06%          97.22%          97.02%
1.50,  0.1                      97.49%          98.14%          98.12%          98.23%          98.06%
1.50, 0.25                      98.81%          99.09%          99.07%          99.10%          98.99%

 */

fn main() {
    println!("zipf & cache size\t\tlru\t\tmoka\t\tQuickC\t\tTinyUFO\t\tTinyUFO Compact",);
    for zif_exp in [0.9, 1.0, 1.05, 1.1, 1.5] {
        for cache_capacity in [0.005, 0.01, 0.05, 0.1, 0.25] {
            bench_one(zif_exp, cache_capacity);
        }
    }
}
