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

//! Weighted Selection

use super::{Backend, BackendIter, BackendSelection, SelectionAlgorithm};
use fnv::FnvHasher;
use std::collections::HashSet;
use std::sync::Arc;

/// Weighted selection with a given selection algorithm
///
/// The default algorithm is [FnvHasher]. See [super::algorithms] for more choices.
pub struct Weighted<M, H = FnvHasher> {
    backends: Box<[Backend<M>]>,
    // each item is an index to the `backends`, use u16 to save memory, support up to 2^16 backends
    weighted: Box<[u16]>,
    algorithm: H,
}

impl<M, H: SelectionAlgorithm> BackendSelection for Weighted<M, H>
where
    M: Clone,
{
    type Metadata = M;
    type Iter = WeightedIterator<M, H>;

    fn build(backends: &HashSet<Backend<M>>) -> Self {
        assert!(
            backends.len() <= u16::MAX as usize,
            "support up to 2^16 backends"
        );
        let mut backends = Vec::from_iter(backends.iter().cloned()).into_boxed_slice();
        backends.sort_unstable_by(|a, b| {
            // Sort "backwards" to ensure that we get "heaviest" backends at
            // the front of the list
            b.weight.cmp(&a.weight)
        });

        let mut weighted = Vec::with_capacity(backends.len());
        for (index, b) in backends.iter().enumerate() {
            for _ in 0..b.weight {
                weighted.push(index as u16);
            }
        }
        Weighted {
            backends,
            weighted: weighted.into_boxed_slice(),
            algorithm: H::new(),
        }
    }

    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter {
        WeightedIterator::new(key, self.clone())
    }
}

/// An iterator over the backends of a [Weighted] selection.
///
/// See [super::BackendSelection] for more information.
pub struct WeightedIterator<M, H> {
    // the unbounded index seed
    index: u64,
    backend: Arc<Weighted<M, H>>,
    first: bool,
}

impl<M, H: SelectionAlgorithm> WeightedIterator<M, H> {
    /// Constructs a new [WeightedIterator].
    fn new(input: &[u8], backend: Arc<Weighted<M, H>>) -> Self {
        Self {
            index: backend.algorithm.next(input),
            backend,
            first: true,
        }
    }
}

impl<M, H: SelectionAlgorithm> BackendIter for WeightedIterator<M, H> {
    type Metadata = M;

    fn next(&mut self) -> Option<&Backend<M>> {
        if self.backend.backends.is_empty() {
            // short circuit if empty
            return None;
        }

        if self.first {
            // initial hash, select from the weighted list
            self.first = false;
            let len = self.backend.weighted.len();
            let index = self.backend.weighted[self.index as usize % len];
            Some(&self.backend.backends[index as usize])
        } else {
            // fallback, select from the unique list
            // deterministically select the next item
            self.index = self.backend.algorithm.next(&self.index.to_le_bytes());
            let len = self.backend.backends.len();
            Some(&self.backend.backends[self.index as usize % len])
        }
    }
}

#[cfg(test)]
mod test {
    use super::super::algorithms::*;
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_fnv() {
        let b1 = Backend::new_with_meta("1.1.1.1:80", 1u32).unwrap();
        let mut b2 = Backend::new_with_meta("1.0.0.1:80", 2u32).unwrap();
        b2.weight = 10; // 10x than the rest
        let b3 = Backend::new_with_meta("1.0.0.255:80", 3u32).unwrap();
        let backends = HashSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash: Arc<Weighted<_>> = Arc::new(Weighted::build(&backends));

        // NOTE: We use a HashSet, which has random iteration order. For this reason,
        // we use the `backends` field instead of the b1/b2/b3 index, as they may have
        // gotten shuffled randomly while building!
        //
        // However, we CAN rely on the fact that `b2` is the "most weighted" option.

        // same hash iter over
        let mut iter = hash.iter(b"test");
        // first, should be weighted
        assert_eq!(iter.next(), Some(&b2));
        // fallbacks, should be uniform, not weighted
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        assert_eq!(iter.next(), Some(&hash.backends[2]));
        assert_eq!(iter.next(), Some(&hash.backends[1]));
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        assert_eq!(iter.next(), Some(&hash.backends[2]));
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        assert_eq!(iter.next(), Some(&hash.backends[1]));
        assert_eq!(iter.next(), Some(&hash.backends[2]));

        // different hashes, the first selection should be weighted
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test2");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test3");
        assert_eq!(iter.next(), Some(&hash.backends[1]));
        let mut iter = hash.iter(b"test4");
        assert_eq!(iter.next(), Some(&hash.backends[2]));
        let mut iter = hash.iter(b"test5");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test6");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test7");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
    }

    #[test]
    fn test_round_robin() {
        let b1 = Backend::new_with_meta("1.1.1.1:80", 1u32).unwrap();
        let mut b2 = Backend::new_with_meta("1.0.0.1:80", 2u32).unwrap();
        b2.weight = 8; // 8x than the rest
        let b3 = Backend::new_with_meta("1.0.0.255:80", 3u32).unwrap();
        let backends = HashSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash: Arc<Weighted<_, RoundRobin>> = Arc::new(Weighted::build(&backends));

        // NOTE: We use a HashSet, which has random iteration order. For this reason,
        // we use the `backends` field instead of the b1/b2/b3 index, as they may have
        // gotten shuffled randomly while building!
        //
        // However, we CAN rely on the fact that `b2` is the "most weighted" option.

        // same hash iter over
        let mut iter = hash.iter(b"test");

        // first, should be weighted
        assert_eq!(iter.next(), Some(&b2));
        // fallbacks, should be round robin
        assert_eq!(iter.next(), Some(&hash.backends[1]));
        assert_eq!(iter.next(), Some(&hash.backends[2]));
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        assert_eq!(iter.next(), Some(&hash.backends[1]));

        // round robin, ignoring the hash key
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[1]));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[2]));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&hash.backends[0]));
    }

    #[test]
    fn test_random() {
        let b1 = Backend::new_with_meta("1.1.1.1:80", 100u32).unwrap();
        let mut b2 = Backend::new_with_meta("1.0.0.1:80", 100u32).unwrap();
        b2.weight = 8; // 8x than the rest
        let b3 = Backend::new_with_meta("1.0.0.255:80", 100u32).unwrap();
        let backends = HashSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash: Arc<Weighted<_, Random>> = Arc::new(Weighted::build(&backends));

        let mut count = HashMap::new();
        count.insert(b1.clone(), 0);
        count.insert(b2.clone(), 0);
        count.insert(b3.clone(), 0);

        for _ in 0..100 {
            let mut iter = hash.iter(b"test");
            *count.get_mut(iter.next().unwrap()).unwrap() += 1;
        }
        let b2_count = *count.get(&b2).unwrap();
        assert!((70..=90).contains(&b2_count));
    }
}
