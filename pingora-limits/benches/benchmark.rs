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

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use ahash::RandomState;
use dashmap::DashMap;
use pingora_limits::estimator::Estimator;
use rand::distributions::Uniform;
use rand::{thread_rng, Rng};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

trait Counter {
    fn incr(&self, key: u32, value: usize);
    fn name() -> &'static str;
}

#[derive(Default)]
struct NaiveCounter(Mutex<HashMap<u32, usize>>);
impl Counter for NaiveCounter {
    fn incr(&self, key: u32, value: usize) {
        let mut map = self.0.lock().unwrap();
        if let Some(v) = map.get_mut(&key) {
            *v += value;
        } else {
            map.insert(key, value);
        }
    }

    fn name() -> &'static str {
        "Naive Counter"
    }
}

#[derive(Default)]
struct OptimizedCounter(DashMap<u32, AtomicUsize, RandomState>);
impl Counter for OptimizedCounter {
    fn incr(&self, key: u32, value: usize) {
        if let Some(v) = self.0.get(&key) {
            v.fetch_add(value, Ordering::Relaxed);
            return;
        }
        self.0.insert(key, AtomicUsize::new(value));
    }

    fn name() -> &'static str {
        "Optimized Counter"
    }
}

impl Counter for Estimator {
    fn incr(&self, key: u32, value: usize) {
        self.incr(key, value as isize);
    }

    fn name() -> &'static str {
        "Pingora Estimator"
    }
}

fn run_bench<T: Counter>(
    counter: &T,
    samples: usize,
    distribution: &Uniform<u32>,
    test_name: &str,
) {
    let mut rng = thread_rng();
    let before = Instant::now();
    for _ in 0..samples {
        let event: u32 = rng.sample(distribution);
        counter.incr(event, 1);
    }
    let elapsed = before.elapsed();
    println!(
        "{} {test_name} {:?} total, {:?} avg per operation",
        T::name(),
        elapsed,
        elapsed / samples as u32
    );
}

fn run_threaded_bench<T: Counter + Send + Sync + 'static>(
    threads: usize,
    counter: Arc<T>,
    samples: usize,
    distribution: &Uniform<u32>,
) {
    let mut handlers = vec![];
    for i in 0..threads {
        let est = counter.clone();
        let dist = *distribution;
        let handler = thread::spawn(move || {
            run_bench(est.as_ref(), samples, &dist, &format!("thread#{i}"));
        });
        handlers.push(handler);
    }
    for thread in handlers {
        thread.join().unwrap();
    }
}

/*
Pingora Estimator single thread 1.042849543s total, 10ns avg per operation
Naive Counter single thread 5.12641496s total, 51ns avg per operation
Optimized Counter single thread 4.302553352s total, 43ns avg per operation
Pingora Estimator thread#7 2.654667606s total, 212ns avg per operation
Pingora Estimator thread#2 2.65651993s total, 212ns avg per operation
Pingora Estimator thread#4 2.658225266s total, 212ns avg per operation
Pingora Estimator thread#0 2.660603361s total, 212ns avg per operation
Pingora Estimator thread#1 2.66139014s total, 212ns avg per operation
Pingora Estimator thread#6 2.663498849s total, 213ns avg per operation
Pingora Estimator thread#5 2.663344276s total, 213ns avg per operation
Pingora Estimator thread#3 2.664652951s total, 213ns avg per operation
Naive Counter thread#7 18.795881242s total, 1.503µs avg per operation
Naive Counter thread#1 18.805652672s total, 1.504µs avg per operation
Naive Counter thread#6 18.818084416s total, 1.505µs avg per operation
Naive Counter thread#4 18.832778982s total, 1.506µs avg per operation
Naive Counter thread#3 18.833952715s total, 1.506µs avg per operation
Naive Counter thread#2 18.837975133s total, 1.507µs avg per operation
Naive Counter thread#0 18.8397464s total, 1.507µs avg per operation
Naive Counter thread#5 18.842616299s total, 1.507µs avg per operation
Optimized Counter thread#4 2.650860314s total, 212ns avg per operation
Optimized Counter thread#0 2.651867013s total, 212ns avg per operation
Optimized Counter thread#2 2.656473381s total, 212ns avg per operation
Optimized Counter thread#5 2.657715876s total, 212ns avg per operation
Optimized Counter thread#1 2.658275111s total, 212ns avg per operation
Optimized Counter thread#7 2.658770751s total, 212ns avg per operation
Optimized Counter thread#6 2.659831251s total, 212ns avg per operation
Optimized Counter thread#3 2.664375398s total, 213ns avg per operation
*/

/* cargo bench --features dhat-heap for memory info

Pingora Estimator single thread 1.066846098s total, 10ns avg per operation
dhat: Total:     26,184 bytes in 9 blocks
dhat: At t-gmax: 26,184 bytes in 9 blocks
dhat: At t-end:  1,464 bytes in 5 blocks
dhat: The data has been saved to dhat-heap.json, and is viewable with dhat/dh_view.html
Naive Counter single thread 5.429089242s total, 54ns avg per operation
dhat: Total:     71,303,260 bytes in 20 blocks
dhat: At t-gmax: 53,477,392 bytes in 2 blocks
dhat: At t-end:  0 bytes in 0 blocks
dhat: The data has been saved to dhat-heap.json, and is viewable with dhat/dh_view.html
Optimized Counter single thread 4.361720355s total, 43ns avg per operation
dhat: Total:     71,307,722 bytes in 491 blocks
dhat: At t-gmax: 36,211,208 bytes in 34 blocks
dhat: At t-end:  0 bytes in 0 blocks
dhat: The data has been saved to dhat-heap.json, and is viewable with dhat/dh_view.html
*/

fn main() {
    const SAMPLES: usize = 100_000_000;
    const THREADS: usize = 8;
    const ITEMS: u32 = 1_000_000;
    const SAMPLES_PER_THREAD: usize = SAMPLES / THREADS;
    let distribution = Uniform::new(0, ITEMS);

    // single thread
    {
        #[cfg(feature = "dhat-heap")]
        let _profiler = dhat::Profiler::new_heap();
        let pingora_est = Estimator::new(3, 1024);
        run_bench(&pingora_est, SAMPLES, &distribution, "single thread");
    }

    {
        #[cfg(feature = "dhat-heap")]
        let _profiler = dhat::Profiler::new_heap();
        let naive: NaiveCounter = Default::default();
        run_bench(&naive, SAMPLES, &distribution, "single thread");
    }

    {
        #[cfg(feature = "dhat-heap")]
        let _profiler = dhat::Profiler::new_heap();
        let optimized: OptimizedCounter = Default::default();
        run_bench(&optimized, SAMPLES, &distribution, "single thread");
    }

    // multithread
    let pingora_est = Arc::new(Estimator::new(3, 1024));
    run_threaded_bench(THREADS, pingora_est, SAMPLES_PER_THREAD, &distribution);

    let naive: Arc<NaiveCounter> = Default::default();
    run_threaded_bench(THREADS, naive, SAMPLES_PER_THREAD, &distribution);

    let optimized: Arc<OptimizedCounter> = Default::default();
    run_threaded_bench(THREADS, optimized, SAMPLES_PER_THREAD, &distribution);
}
