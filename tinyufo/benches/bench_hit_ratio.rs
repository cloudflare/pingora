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

    let mut rng = thread_rng();
    let zipf = zipf::ZipfDistribution::new(ITEMS, zip_exp).unwrap();

    let mut lru_hit = 0;
    let mut moka_hit = 0;
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

        if tinyufo.get(&key).is_some() {
            tinyufo_hit += 1;
        } else {
            tinyufo.put(key, (), 1);
        }
    }

    print!("{:.2}%\t\t", lru_hit as f32 / ITERATIONS as f32 * 100.0);
    print!("{:.2}%\t\t", moka_hit as f32 / ITERATIONS as f32 * 100.0);
    println!("{:.2}%", tinyufo_hit as f32 / ITERATIONS as f32 * 100.0);
}

/*
cargo bench --bench bench_hit_ratio

zipf & cache size               lru             moka            TinyUFO
0.90, 0.005                     19.23%          33.46%          33.35%
0.90, 0.01                      26.21%          37.88%          40.10%
0.90, 0.05                      45.59%          55.34%          57.81%
0.90,  0.1                      55.73%          64.22%          66.34%
0.90, 0.25                      71.18%          77.15%          78.53%
1.00, 0.005                     31.09%          45.65%          45.13%
1.00, 0.01                      39.17%          50.69%          52.23%
1.00, 0.05                      58.73%          66.95%          68.81%
1.00,  0.1                      67.57%          74.35%          75.93%
1.00, 0.25                      79.91%          84.34%          85.27%
1.05, 0.005                     37.68%          51.77%          51.26%
1.05, 0.01                      46.11%          57.07%          58.41%
1.05, 0.05                      65.04%          72.33%          73.91%
1.05,  0.1                      73.11%          78.96%          80.22%
1.05, 0.25                      83.77%          87.45%          88.16%
1.10, 0.005                     44.48%          57.86%          57.25%
1.10, 0.01                      52.97%          63.18%          64.23%
1.10, 0.05                      70.94%          77.27%          78.57%
1.10,  0.1                      78.11%          83.05%          84.06%
1.10, 0.25                      87.08%          90.06%          90.62%
1.50, 0.005                     85.25%          89.89%          89.68%
1.50, 0.01                      89.88%          92.79%          92.94%
1.50, 0.05                      96.04%          97.09%          97.25%
1.50,  0.1                      97.52%          98.17%          98.26%
1.50, 0.25                      98.81%          99.09%          99.10%
 */

fn main() {
    println!("zipf & cache size\t\tlru\t\tmoka\t\tTinyUFO",);
    for zif_exp in [0.9, 1.0, 1.05, 1.1, 1.5] {
        for cache_capacity in [0.005, 0.01, 0.05, 0.1, 0.25] {
            bench_one(zif_exp, cache_capacity);
        }
    }
}
