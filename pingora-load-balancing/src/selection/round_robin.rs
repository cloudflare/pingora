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

//! Smooth Round Robin

use super::*;
use std::sync::atomic::{AtomicIsize, Ordering};

/// Smooth Round Robin.
/// It provides a smooth round robin algorighm which is identical in behavior to [nginx smooth weighted round-robin balancing](https://github.com/nginx/nginx/commit/52327e0627f49dbda1e8db695e63a4b0af4448b1).
pub struct SmoothRoundRobin {
    buckets: Vec<Bucket<Backend>>,
    total: usize,
}

#[derive(Debug)]
struct Bucket<T> {
    item: T,
    weight: isize,
    current_weight: AtomicIsize,
}

impl BackendSelection for SmoothRoundRobin {
    type Iter = SmoothIterator;

    fn build(backends: &BTreeSet<Backend>) -> Self {
        let buckets: Vec<_> = backends
            .iter()
            .map(|b| Bucket {
                item: b.clone(),
                weight: b.weight as isize,
                current_weight: AtomicIsize::new(0),
            })
            .collect();

        let total = backends.iter().map(|b| b.weight).sum();
        Self { buckets, total }
    }

    fn iter(self: &Arc<Self>, _key: &[u8]) -> Self::Iter {
        SmoothIterator { ring: self.clone() }
    }
}

impl SmoothRoundRobin {
    fn next(&self) -> Option<usize> {
        if self.buckets.len() == 0 {
            return None;
        }

        let mut best_index = None;
        let mut best_weight = 0;

        for (idx, bucket) in self.buckets.iter().enumerate() {
            let weight = bucket
                .current_weight
                .fetch_add(bucket.weight, Ordering::Relaxed);

            if best_index.is_none() || weight > best_weight {
                best_index = Some(idx);
                best_weight = weight;
            }
        }

        if let Some(idx) = best_index {
            self.buckets[idx]
                .current_weight
                .fetch_sub(self.total as isize, Ordering::Relaxed);
        }
        best_index
    }
}

/// Iterator over a SmoothRoundRobin.
pub struct SmoothIterator {
    ring: Arc<SmoothRoundRobin>,
}

impl BackendIter for SmoothIterator {
    fn next(&mut self) -> Option<&Backend> {
        self.ring
            .next()
            .and_then(|idx| self.ring.buckets.get(idx))
            .map(|b| &b.item)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_smooth_round_robin() {
        let mut b1 = Backend::new("1.1.1.1:80").unwrap();
        b1.weight = 60;
        let mut b2 = Backend::new("1.1.1.2:80").unwrap();
        b2.weight = 20;
        let mut b3 = Backend::new("1.1.1.3:80").unwrap();
        b3.weight = 20;

        let mut counter = HashMap::new();
        let backends = BTreeSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash = Arc::new(SmoothRoundRobin::build(&backends));

        // same hash iter over
        let hash = hash.clone();
        for _ in 0..100 {
            let mut iter = hash.iter(b"test");
            let b = iter.next().unwrap();
            let c = counter.entry(b.addr.clone().to_string()).or_insert(0);
            *c += 1;
        }

        assert_eq!(*counter.get("1.1.1.1:80").unwrap(), 60);
        assert_eq!(*counter.get("1.1.1.2:80").unwrap(), 20);
        assert_eq!(*counter.get("1.1.1.3:80").unwrap(), 20);
    }
}
