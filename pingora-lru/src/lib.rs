// Copyright 2026 Cloudflare, Inc.
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

//! An implementation of an LRU that focuses on memory efficiency, concurrency and persistence
//!
//! Features
//! - keys can have different sizes
//! - LRUs are sharded to avoid global locks.
//! - Memory layout and usage are optimized: small and no memory fragmentation

pub mod async_lru;
pub mod linked_list;
pub mod persistence;

use linked_list::LinkedListIter;

/// The old lock-based code uses `LinkedList<u64>`.
type LinkedList = linked_list::LinkedList<u64>;

use hashbrown::HashMap;
use parking_lot::RwLock;
use rand::Rng;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Where [`Lru::mutate_pair`] inserts a key that did not previously exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    /// Most recently used position.
    Head,
    /// Least recently used position.
    #[default]
    Tail,
}

/// Outcome of [`Lru::upsert_if`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Upsert {
    /// An accepted existing entry was updated; carries its previous weight.
    Updated(usize),
    /// A missing entry was admitted.
    Admitted,
    /// An existing entry was rejected and left unchanged.
    Rejected,
}

/// The LRU with `N` shards
pub struct Lru<T, const N: usize> {
    units: [RwLock<LruUnit<T>>; N],
    /// Lock-free `Relaxed` shadow of each shard's item count, backing
    /// [`Lru::shard_len`] and the P2C selection in [`Lru::evict_to_limit`].
    /// Maintained alongside [`Lru::len`] at every count-mutating site.
    shard_lens: [AtomicUsize; N],
    weight: AtomicUsize,
    weight_limit: AtomicUsize,
    len_watermark: Option<usize>,
    len: AtomicUsize,
    evicted_weight: AtomicUsize,
    evicted_len: AtomicUsize,
}

impl<T, const N: usize> Lru<T, N> {
    /// Create an [Lru] with the given weight limit and predicted capacity.
    ///
    /// The capacity is per shard (for simplicity). So the total capacity = capacity * N
    pub fn with_capacity(weight_limit: usize, capacity: usize) -> Self {
        Self::with_capacity_and_watermark(weight_limit, capacity, None)
    }

    /// Create an [Lru] with the given weight limit, predicted capacity and optional watermark
    ///
    /// The capacity is per shard (for simplicity). So the total capacity = capacity * N
    ///
    /// The watermark indicates at what count we should begin evicting and acts as a limit
    /// on the total number of allowed items.
    pub fn with_capacity_and_watermark(
        weight_limit: usize,
        capacity: usize,
        len_watermark: Option<usize>,
    ) -> Self {
        // use the unsafe code from ArrayVec just to init the array
        let mut units = arrayvec::ArrayVec::<_, N>::new();
        let mut shard_lens = arrayvec::ArrayVec::<_, N>::new();
        for _ in 0..N {
            units.push(RwLock::new(LruUnit::with_capacity(capacity)));
            shard_lens.push(AtomicUsize::new(0));
        }
        Lru {
            units: units.into_inner().map_err(|_| "").unwrap(),
            shard_lens: shard_lens
                .into_inner()
                .expect("shard_lens ArrayVec filled with exactly N elements"),
            weight: AtomicUsize::new(0),
            weight_limit: AtomicUsize::new(weight_limit),
            len_watermark,
            len: AtomicUsize::new(0),
            evicted_weight: AtomicUsize::new(0),
            evicted_len: AtomicUsize::new(0),
        }
    }

    /// Return the current total weight limit.
    pub fn weight_limit(&self) -> usize {
        self.weight_limit.load(Ordering::Relaxed)
    }

    /// Set the total weight limit used by [`Self::evict_to_limit`].
    pub fn set_weight_limit(&self, weight_limit: usize) {
        self.weight_limit.store(weight_limit, Ordering::Relaxed);
    }

    /// Increment item-count bookkeeping for `shard`. Both atomics use
    /// `Relaxed`; called while holding the shard write lock so that
    /// `len` and `shard_lens[shard]` advance in lockstep.
    #[inline]
    fn incr_count(&self, shard: usize) {
        self.len.fetch_add(1, Ordering::Relaxed);
        self.shard_lens[shard].fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement item-count bookkeeping for `shard`. See
    /// [`Self::incr_count`].
    #[inline]
    fn decr_count(&self, shard: usize) {
        self.len.fetch_sub(1, Ordering::Relaxed);
        self.shard_lens[shard].fetch_sub(1, Ordering::Relaxed);
    }

    /// Move the global weight from `old_weight` to `new_weight` with one atomic
    /// read-modify-write.
    ///
    /// The change may go in either direction. Applying the net delta as a single
    /// `fetch_add` or `fetch_sub` means a concurrent reader of [`Self::weight`] sees
    /// this update either not at all or in full; two separate updates would expose
    /// an intermediate total that no entry state ever had.
    ///
    /// `Relaxed` is enough: [`Self::weight`] is a best-effort shadow that publishes
    /// no other data, so the update needs atomicity, not ordering.
    #[inline]
    fn apply_weight_change(&self, old_weight: usize, new_weight: usize) {
        if new_weight >= old_weight {
            self.weight
                .fetch_add(new_weight - old_weight, Ordering::Relaxed);
        } else {
            self.weight
                .fetch_sub(old_weight - new_weight, Ordering::Relaxed);
        }
    }

    /// Admit the key value to the [Lru]
    ///
    /// Return the shard index which the asset is added to
    pub fn admit(&self, key: u64, data: T, weight: usize) -> usize {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();

        // Make sure weight is positive otherwise eviction won't work
        // TODO: Probably should use NonZeroUsize instead
        let weight = weight.max(1);

        let old_weight = unit.admit(key, data, weight);
        if old_weight != weight {
            if old_weight > 0 {
                self.apply_weight_change(old_weight, weight);
            } else {
                // Assume old_weight == 0 means a new item is admitted
                self.weight.fetch_add(weight, Ordering::Relaxed);
                self.incr_count(shard);
            }
        }
        shard
    }

    /// Increment the weight associated with a given key, up to an optional max weight.
    ///
    /// If the key does not exist, it is admitted with data from `admit_data` and a weight
    /// equal to `delta`, capped to `max_weight` when provided, and floored to 1. The
    /// admission data is only constructed for missing keys; existing entries keep their
    /// stored data unchanged.
    ///
    /// If a `max_weight` is provided, the weight cannot grow beyond this max weight. If the
    /// current weight is higher than the max, the current weight is retained.
    ///
    /// Return the total new weight.
    pub fn increment_weight<F>(
        &self,
        key: u64,
        admit_data: F,
        delta: usize,
        max_weight: Option<usize>,
    ) -> usize
    where
        F: FnOnce() -> T,
    {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();
        let (old_weight, new_weight, admitted) =
            unit.increment_weight(key, admit_data, delta, max_weight);
        debug_assert!(new_weight >= old_weight);
        self.weight
            .fetch_add(new_weight - old_weight, Ordering::Relaxed);
        if admitted {
            self.incr_count(shard);
        }
        new_weight
    }

    /// Update an existing entry while holding its shard write lock, or admit a
    /// new entry if it is not present.
    ///
    /// Existing entries are passed to `update` with their current data and
    /// weight. The returned weight is floored to `1`, the entry is promoted,
    /// and the global weight is adjusted while the shard mutation is still
    /// exclusive.
    ///
    /// Missing entries call `admit_data` to construct the data and initial
    /// weight, insert the entry at the head, and floor the weight to `1`.
    ///
    /// Returns the old weight when the entry existed, or `None` when the
    /// entry was newly admitted.
    pub fn update_or_admit<U, A>(&self, key: u64, update: U, admit_data: A) -> Option<usize>
    where
        U: FnOnce(&mut T, usize) -> usize,
        A: FnOnce() -> (T, usize),
    {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();
        let (old_weight, new_weight) = unit.update_or_admit(key, update, admit_data);
        match old_weight {
            Some(old_weight) => {
                if old_weight != new_weight {
                    self.apply_weight_change(old_weight, new_weight);
                }
            }
            None => {
                self.weight.fetch_add(new_weight, Ordering::Relaxed);
                self.incr_count(shard);
            }
        }
        old_weight
    }

    /// Update an existing entry only if `predicate` accepts its data.
    ///
    /// The predicate, update, optional promotion, and weight accounting run under
    /// the shard write lock. Callbacks must not re-enter this shard. The weight
    /// returned by `update` is floored to `1`. Returns the old weight, or `None`
    /// if the key is absent or rejected, leaving data, weight, and order unchanged.
    pub fn update_if(
        &self,
        key: u64,
        predicate: impl FnOnce(&T) -> bool,
        update: impl FnOnce(&mut T, usize) -> usize,
        promote: bool,
    ) -> Option<usize> {
        let shard = get_shard(key, N);
        let unit = &mut *self.units[shard].write();
        let node = unit.lookup_table.get_mut(&key)?;
        if !predicate(&node.data) {
            return None;
        }
        let new_weight = update(&mut node.data, node.weight).max(1);
        let old_weight = LruUnit::adjust_weight(node, &mut unit.used_weight, new_weight);
        if promote {
            unit.order.promote(node.list_index);
        }
        if old_weight != new_weight {
            self.weight.fetch_add(new_weight, Ordering::Relaxed);
            self.weight.fetch_sub(old_weight, Ordering::Relaxed);
        }
        Some(old_weight)
    }

    /// Update an accepted existing entry, or admit a missing entry.
    ///
    /// Like [`Self::update_or_admit`], accepted updates promote the entry and all
    /// weights are floored to `1`. Returns [`Upsert::Updated`] for an update,
    /// [`Upsert::Admitted`] for admission, or [`Upsert::Rejected`] when an
    /// existing entry fails `predicate` without changing data, weight, order, or counters.
    /// `predicate` is not called for missing keys. All callbacks and accounting
    /// run under the shard write lock; callbacks must not re-enter this shard.
    pub fn upsert_if(
        &self,
        key: u64,
        predicate: impl FnOnce(&T) -> bool,
        update: impl FnOnce(&mut T, usize) -> usize,
        admit_data: impl FnOnce() -> (T, usize),
    ) -> Upsert {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();
        if let Some(data) = unit.peek(key) {
            if !predicate(data) {
                return Upsert::Rejected;
            }
        }
        let (old_weight, new_weight) = unit.update_or_admit(key, update, admit_data);
        match old_weight {
            Some(old_weight) => {
                if old_weight != new_weight {
                    self.weight.fetch_add(new_weight, Ordering::Relaxed);
                    self.weight.fetch_sub(old_weight, Ordering::Relaxed);
                }
            }
            None => {
                self.weight.fetch_add(new_weight, Ordering::Relaxed);
                self.incr_count(shard);
            }
        }
        match old_weight {
            Some(weight) => Upsert::Updated(weight),
            None => Upsert::Admitted,
        }
    }

    /// Mutate two distinct keys atomically under their existing shard locks.
    ///
    /// Locks are acquired in ascending shard order; keys in the same shard take
    /// a single lock. The callback receives cloned data and weights; assigning
    /// `None` removes a key, and assigning `Some` inserts or updates it. Existing
    /// entries retain their order; new entries use the placement selected by the
    /// callback (the tail by default), and
    /// weights are floored to one. Callbacks must not re-enter this LRU.
    /// Returns `None` without calling the callback when the keys are identical.
    /// Weight increases are published before decreases, so a transfer cannot
    /// transiently understate the global weight. Global counters remain
    /// best-effort observations, not a cross-shard snapshot.
    pub fn mutate_pair<R>(
        &self,
        first: u64,
        second: u64,
        mutate: impl FnOnce(&mut Option<(T, usize)>, &mut Option<(T, usize)>, &mut Placement) -> R,
    ) -> Option<R>
    where
        T: Clone,
    {
        if first == second {
            return None;
        }
        let a = get_shard(first, N);
        let b = get_shard(second, N);
        let low = a.min(b);
        let high = a.max(b);
        let mut low_unit = self.units[low].write();
        let mut high_unit = (low != high).then(|| self.units[high].write());
        let read = |key, shard| {
            let unit = match high_unit.as_deref() {
                Some(unit) if shard != low => unit,
                _ => &*low_unit,
            };
            unit.lookup_table
                .get(&key)
                .map(|node| (node.data.clone(), node.weight))
        };
        let mut left = read(first, a);
        let mut right = read(second, b);
        let old_left = left.as_ref().map_or(0, |(_, weight)| *weight);
        let old_right = right.as_ref().map_or(0, |(_, weight)| *weight);
        let existed_left = left.is_some();
        let existed_right = right.is_some();
        let mut placement = Placement::default();
        let result = mutate(&mut left, &mut right, &mut placement);
        for value in [&mut left, &mut right].into_iter().flatten() {
            value.1 = value.1.max(1);
        }
        let new_left = left.as_ref().map_or(0, |(_, weight)| *weight);
        let new_right = right.as_ref().map_or(0, |(_, weight)| *weight);
        self.weight
            .fetch_add(new_left.saturating_sub(old_left), Ordering::Relaxed);
        self.weight
            .fetch_add(new_right.saturating_sub(old_right), Ordering::Relaxed);
        for (key, shard, value, existed) in [
            (first, a, left, existed_left),
            (second, b, right, existed_right),
        ] {
            let unit = match high_unit.as_deref_mut() {
                Some(unit) if shard != low => unit,
                _ => &mut *low_unit,
            };
            match value {
                Some((data, weight)) => {
                    if let Some(node) = unit.lookup_table.get_mut(&key) {
                        LruUnit::adjust_weight(node, &mut unit.used_weight, weight);
                        node.data = data;
                    } else {
                        unit.insert_at(key, data, weight, placement);
                        self.incr_count(shard);
                    }
                }
                None if existed => {
                    unit.remove(key);
                    self.decr_count(shard);
                }
                None => {}
            }
        }
        self.weight
            .fetch_sub(old_left.saturating_sub(new_left), Ordering::Relaxed);
        self.weight
            .fetch_sub(old_right.saturating_sub(new_right), Ordering::Relaxed);
        Some(result)
    }

    /// Set the weight associated with an existing key without changing LRU order.
    ///
    /// Missing keys are left unchanged and return `None`. The weight is floored
    /// to `1`.
    ///
    /// Return the old weight if the key exists.
    pub fn set_weight(&self, key: u64, weight: usize) -> Option<usize> {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();

        let weight = weight.max(1);
        let old_weight = unit.set_weight(key, weight)?;
        if old_weight != weight {
            self.apply_weight_change(old_weight, weight);
        }
        Some(old_weight)
    }

    /// Set an existing key's weight if its current weight equals `expected_weight`.
    ///
    /// The comparison and update share the shard write lock. Missing keys or weight
    /// mismatches return `false` without changes. A match returns `true`, floors the
    /// new weight to `1`, and preserves the entry's data and LRU order.
    ///
    /// This never admits a missing key and never evicts, so growing an entry can
    /// leave [`Self::weight`] above [`Self::weight_limit`] until the caller runs
    /// [`Self::evict_to_limit`].
    #[must_use = "the weight is unchanged when this returns `false`"]
    pub fn set_weight_if_unchanged(&self, key: u64, expected_weight: usize, weight: usize) -> bool {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();

        let weight = weight.max(1);
        if !unit.set_weight_if_unchanged(key, expected_weight, weight) {
            return false;
        }
        if expected_weight != weight {
            self.apply_weight_change(expected_weight, weight);
        }
        true
    }

    /// Promote the key to the head of the LRU
    ///
    /// Return `true` if the key exists.
    pub fn promote(&self, key: u64) -> bool {
        self.units[get_shard(key, N)].write().access(key)
    }

    /// Promote to the top n of the LRU
    ///
    /// This function acquires a read lock first to check if the key is already
    /// in the top `n` positions. If so, it returns early without a write lock.
    /// Otherwise it falls through to a write lock for the actual promotion.
    ///
    /// **Performance note**: this optimization only helps when `n` covers a
    /// significant fraction of the shard. At production scale (~100K+ items
    /// per shard), hot items are rarely in the top N positions, so the
    /// read-lock scan is usually wasted work that adds latency without
    /// reducing contention. Benchmarks (`cargo bench --bench bench_lru`)
    /// show that plain [`promote()`](Self::promote) is faster at scale.
    /// Consider using `promote()` directly unless profiling shows a clear
    /// benefit for your workload.
    ///
    /// Return false if the item doesn't exist
    pub fn promote_top_n(&self, key: u64, top: usize) -> bool {
        let unit = &self.units[get_shard(key, N)];
        if !unit.read().need_promote(key, top) {
            return true;
        }
        unit.write().access(key)
    }

    /// Evict at most one item from the given shard, identified by the
    /// hash-like `shard` seed (mapped into `0..N` via `% N`).
    ///
    /// Return the evicted asset and its size if there is anything to evict.
    pub fn evict_shard(&self, shard: u64) -> Option<(T, usize)> {
        self.evict_shard_at(get_shard(shard, N))
    }

    /// Evict at most one item from the shard at index `shard` (in `0..N`).
    /// Internal entry point that skips the `% N` round-trip in
    /// [`Self::evict_shard`].
    fn evict_shard_at(&self, shard: usize) -> Option<(T, usize)> {
        assert!(shard < N);
        let evicted = self.units[shard].write().evict();
        if let Some((_, weight)) = evicted.as_ref() {
            self.weight.fetch_sub(*weight, Ordering::Relaxed);
            self.decr_count(shard);
            self.evicted_weight.fetch_add(*weight, Ordering::Relaxed);
            self.evicted_len.fetch_add(1, Ordering::Relaxed);
        }
        evicted
    }

    /// Evict the [Lru] until the overall weight is below the limit (or the configured watermark).
    ///
    /// Return a list of evicted items.
    ///
    /// Each iteration selects the shard to evict from using the "power of two
    /// choices" strategy: two shards are picked uniformly at random and the
    /// one with more items is chosen (see
    /// <https://brooker.co.za/blog/2012/01/17/two-random.html>). This biases
    /// eviction toward longer shards and drives [`Self::shard_len`] toward a
    /// uniform distribution, which keeps per-shard serialization cost (e.g.
    /// `pingora_cache::eviction::lru::Manager::serialize_shard`) bounded.
    ///
    /// Selection is by item count, not weight, even when eviction is
    /// triggered by `weight_limit`. With heavily skewed item weights this
    /// may evict more items than a weight-biased policy to reach the same
    /// total weight — the tradeoff is intentional in favor of bounded
    /// per-shard serialization cost.
    ///
    /// O(1) per iteration in the common case. If the chosen shard is
    /// empty when we acquire its write lock (the Relaxed shadow may
    /// not always reflect actual emptiness, and P2C may tie-break to
    /// an empty shard when all shadow lengths are equal), we linearly
    /// probe successive shard indices until one yields an item or we
    /// wrap back to the starting shard — at which point every shard
    /// was observed empty and we exit. Bounded by at most N probes
    /// per outer iteration.
    pub fn evict_to_limit(&self) -> Vec<(T, usize)> {
        self.evict_to_limit_with_rng(&mut rand::thread_rng())
    }

    /// Internal entry point for [`Self::evict_to_limit`] that lets tests
    /// inject a seeded RNG for deterministic P2C selection.
    fn evict_to_limit_with_rng<R: Rng>(&self, rng: &mut R) -> Vec<(T, usize)> {
        let mut evicted = vec![];
        let mut initial_weight = self.weight();
        let mut initial_len = self.len();

        // Transient over-limit weight can persist until the next
        // admit/increment_weight call, which is acceptable because the
        // next admission will re-trigger eviction.
        let weight_limit = self.weight_limit();
        while (initial_weight > weight_limit && self.weight() > weight_limit)
            || self
                .len_watermark
                .is_some_and(|w| initial_len > w && self.len() > w)
        {
            // Power of two choices: pick the longer of two random shards.
            // N == 1 short-circuits the redundant second roll.
            let start = if N <= 1 {
                0
            } else {
                let a = rng.gen_range(0..N);
                let b = rng.gen_range(0..N);
                if self.shard_len(a) >= self.shard_len(b) {
                    a
                } else {
                    b
                }
            };
            // Try the chosen shard first; on a miss (empty or raced),
            // linearly probe successive indices. Wrapping back to
            // `start` means every shard was observed empty, so we exit.
            let mut shard = start;
            let evicted_one = loop {
                if let Some(item) = self.evict_shard_at(shard) {
                    break Some(item);
                }
                shard = (shard + 1) % N;
                if shard == start {
                    break None;
                }
            };
            match evicted_one {
                Some(i) => {
                    initial_weight = initial_weight.saturating_sub(i.1);
                    initial_len = initial_len.saturating_sub(1);
                    evicted.push(i);
                }
                None => break,
            }
        }
        evicted
    }

    /// Remove the given asset.
    pub fn remove(&self, key: u64) -> Option<(T, usize)> {
        let shard = get_shard(key, N);
        let removed = self.units[shard].write().remove(key);
        if let Some((_, weight)) = removed.as_ref() {
            self.weight.fetch_sub(*weight, Ordering::Relaxed);
            self.decr_count(shard);
        }
        removed
    }

    /// Remove an existing entry only if `predicate` accepts its data.
    ///
    /// The predicate, removal, and accounting run under the shard write lock;
    /// the callback must not re-enter this shard. Returns the removed data and
    /// weight, or `None` for an absent or rejected key without changing anything.
    /// Explicit removal does not increment eviction counters; see
    /// [`Self::evict_if`] for a counted removal.
    pub fn remove_if(&self, key: u64, predicate: impl FnOnce(&T) -> bool) -> Option<(T, usize)> {
        self.remove_if_weight(key, |data, _| predicate(data))
    }

    /// Remove an entry only if its data and weight are accepted atomically.
    ///
    /// Like [`Self::remove_if`], the callback and removal run under the shard
    /// write lock. This also permits compare-and-remove against a previously
    /// observed weight without a check-then-remove race.
    pub fn remove_if_weight(
        &self,
        key: u64,
        predicate: impl FnOnce(&T, usize) -> bool,
    ) -> Option<(T, usize)> {
        self.remove_matching(key, predicate, false)
    }

    /// Evict an existing entry only if `predicate` accepts its data.
    ///
    /// Like [`Self::remove_if`], but a removal is counted in
    /// [`Self::evicted_weight`] and [`Self::evicted_len`], as with
    /// [`Self::evict_shard`]. Use this to complete an eviction whose victim was
    /// chosen earlier, for example by [`Self::select_lru_if`]; selection alone
    /// does not count as an eviction because it can be undone. The callback must
    /// not re-enter this shard.
    pub fn evict_if(&self, key: u64, predicate: impl FnOnce(&T) -> bool) -> Option<(T, usize)> {
        self.remove_matching(key, |data, _| predicate(data), true)
    }

    /// Remove an entry accepted by `predicate` under its shard write lock,
    /// optionally counting the removal as an eviction.
    fn remove_matching(
        &self,
        key: u64,
        predicate: impl FnOnce(&T, usize) -> bool,
        count_as_eviction: bool,
    ) -> Option<(T, usize)> {
        let shard = get_shard(key, N);
        let unit = &mut self.units[shard].write();
        let node = unit.lookup_table.get(&key)?;
        if !predicate(&node.data, node.weight) {
            return None;
        }
        let removed = unit.remove(key)?;
        self.weight.fetch_sub(removed.1, Ordering::Relaxed);
        self.decr_count(shard);
        if count_as_eviction {
            self.evicted_weight.fetch_add(removed.1, Ordering::Relaxed);
            self.evicted_len.fetch_add(1, Ordering::Relaxed);
        }
        Some(removed)
    }

    /// Select and promote the least-recently-used eligible entry in a shard.
    ///
    /// The hash-like `shard` seed is mapped into `0..N` via `% N`. The scan
    /// visits at most `max_scan` entries. Ineligible entries visited by the scan
    /// are moved to the head of the shard. On a match, `select` receives the
    /// entry's data and weight, then the entry is promoted. Callbacks must not
    /// re-enter this shard. Returns cloned modified data and unchanged weight
    /// without changing accounting, or `None` if no entry was selected.
    pub fn select_lru_if(
        &self,
        shard: u64,
        max_scan: usize,
        predicate: impl Fn(&T) -> bool,
        select: impl FnOnce(&mut T, usize),
    ) -> Option<(T, usize)>
    where
        T: Clone,
    {
        let unit = &mut *self.units[get_shard(shard, N)].write();
        for _ in 0..max_scan.min(unit.lookup_table.len()) {
            let key = *unit.order.peek(unit.order.tail()?)?;
            let node = unit.lookup_table.get_mut(&key)?;
            if predicate(&node.data) {
                select(&mut node.data, node.weight);
                unit.order.promote(node.list_index);
                return Some((node.data.clone(), node.weight));
            }
            unit.order.promote(node.list_index);
        }
        None
    }

    /// Insert the item to the tail of this LRU.
    ///
    /// Useful to recreate an LRU in most-to-least order
    pub fn insert_tail(&self, key: u64, data: T, weight: usize) -> bool {
        let shard = get_shard(key, N);
        if self.units[shard].write().insert_tail(key, data, weight) {
            self.weight.fetch_add(weight, Ordering::Relaxed);
            self.incr_count(shard);
            true
        } else {
            false
        }
    }

    /// Check existence of a key without changing the order in LRU.
    pub fn peek(&self, key: u64) -> bool {
        self.units[get_shard(key, N)].read().peek(key).is_some()
    }

    /// Inspect the data associated with a key without changing its position in
    /// the LRU.
    ///
    /// The callback runs while holding the shard's read lock. Keep it brief and
    /// do not call methods that would acquire the same shard's write lock.
    pub fn peek_with<F>(&self, key: u64, inspect: F) -> bool
    where
        F: FnOnce(Option<&T>) -> bool,
    {
        inspect(self.units[get_shard(key, N)].read().peek(key))
    }

    /// Check the weight of a key without changing the order in LRU.
    pub fn peek_weight(&self, key: u64) -> Option<usize> {
        self.units[get_shard(key, N)].read().peek_weight(key)
    }

    /// Peek at the least-recently-used item in the given shard without removing it.
    ///
    /// Returns a clone of the data and the weight, or `None` if the shard is empty
    /// or `shard >= N`.
    pub fn peek_lru(&self, shard: usize) -> Option<(T, usize)>
    where
        T: Clone,
    {
        self.units
            .get(shard)?
            .read()
            .peek_lru()
            .map(|(data, weight)| (data.clone(), weight))
    }

    /// Return the current total weight.
    ///
    /// Lock-free `Relaxed` load. Best-effort: not synchronized with
    /// concurrent admissions or evictions on other threads.
    pub fn weight(&self) -> usize {
        self.weight.load(Ordering::Relaxed)
    }

    /// Return the total weight of items evicted from this [Lru].
    pub fn evicted_weight(&self) -> usize {
        self.evicted_weight.load(Ordering::Relaxed)
    }

    /// Return the total count of items evicted from this [Lru].
    pub fn evicted_len(&self) -> usize {
        self.evicted_len.load(Ordering::Relaxed)
    }

    /// The number of items inside this [Lru].
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Scan a shard with the given function F
    pub fn iter_for_each<F>(&self, shard: usize, f: F)
    where
        F: FnMut((&T, usize)),
    {
        assert!(shard < N);
        self.units[shard].read().iter().for_each(f);
    }

    /// Get the total number of shards
    pub const fn shards(&self) -> usize {
        N
    }

    /// Get the number of items inside a shard.
    ///
    /// Lock-free `Relaxed` load from a per-shard atomic shadow. Best-effort:
    /// there is no cross-thread ordering between this and [`Self::len`], and
    /// `Σ shard_len(i)` is not guaranteed to equal [`Self::len`] at any
    /// given instant. Suitable for eviction-balance heuristics and
    /// observability; not suitable for synchronization.
    pub fn shard_len(&self, shard: usize) -> usize {
        self.shard_lens[shard].load(Ordering::Relaxed)
    }

    /// Get the weight (total size) inside a shard
    pub fn shard_weight(&self, shard: usize) -> usize {
        self.units[shard].read().used_weight
    }

    /// Reserve capacity for `additional` more entries in `shard`, avoiding the
    /// reallocation/rehashing of incremental [`Self::insert_tail`] when bulk
    /// loading a shard of known size.
    pub fn reserve_shard(&self, shard: usize, additional: usize) {
        if let Some(unit) = self.units.get(shard) {
            unit.write().reserve(additional);
        }
    }

    /// Capacity of a shard's ordering list (backing node storage).
    #[cfg(test)]
    fn shard_capacity(&self, shard: usize) -> usize {
        self.units[shard].read().order.capacity()
    }

    /// Number of entries a shard's lookup table can hold before it must grow
    /// and rehash.
    #[cfg(test)]
    fn shard_lookup_capacity(&self, shard: usize) -> usize {
        self.units[shard].read().lookup_table.capacity()
    }
}

#[inline]
fn get_shard(key: u64, n_shards: usize) -> usize {
    (key % n_shards as u64) as usize
}

struct LruNode<T> {
    data: T,
    list_index: usize,
    weight: usize,
}

struct LruUnit<T> {
    lookup_table: HashMap<u64, Box<LruNode<T>>>,
    order: LinkedList,
    used_weight: usize,
}

impl<T> LruUnit<T> {
    fn with_capacity(capacity: usize) -> Self {
        LruUnit {
            lookup_table: HashMap::with_capacity(capacity),
            order: LinkedList::with_capacity(capacity),
            used_weight: 0,
        }
    }

    /// Peek data associated with key, if it exists.
    pub fn peek(&self, key: u64) -> Option<&T> {
        self.lookup_table.get(&key).map(|n| &n.data)
    }

    /// Peek weight associated with key, if it exists.
    pub fn peek_weight(&self, key: u64) -> Option<usize> {
        self.lookup_table.get(&key).map(|n| n.weight)
    }

    /// Admit into LRU, return old weight if there was any.
    pub fn admit(&mut self, key: u64, data: T, weight: usize) -> usize {
        if let Some(node) = self.lookup_table.get_mut(&key) {
            let old_weight = Self::adjust_weight(node, &mut self.used_weight, weight);
            node.data = data;
            self.order.promote(node.list_index);
            return old_weight;
        }
        let list_index = self.order.push_head(key);
        self.used_weight += weight;
        let node = Box::new(LruNode {
            data,
            list_index,
            weight,
        });
        self.lookup_table.insert(key, node);
        0
    }

    /// Increase the weight of a key, admitting it if needed.
    ///
    /// `admit_data` is only called when the key is not already tracked. Existing entries
    /// keep their stored data unchanged.
    ///
    /// If a `max_weight` is provided, the weight cannot grow beyond this max weight. If the
    /// current weight is higher than the max, the current weight is retained.
    ///
    /// Returns `(old_weight, new_weight, admitted)`, where `admitted` is true when a new
    /// entry was inserted.
    pub fn increment_weight<F>(
        &mut self,
        key: u64,
        admit_data: F,
        delta: usize,
        max_weight: Option<usize>,
    ) -> (usize, usize, bool)
    where
        F: FnOnce() -> T,
    {
        if let Some(node) = self.lookup_table.get_mut(&key) {
            let incremented = node.weight.saturating_add(delta);
            let new_weight =
                max_weight.map_or(incremented, |m| incremented.min(m).max(node.weight));
            let old_weight = Self::adjust_weight(node, &mut self.used_weight, new_weight);
            self.order.promote(node.list_index);
            return (old_weight, new_weight, false);
        }
        let weight = max_weight.map_or(delta, |m| delta.min(m)).max(1);
        let list_index = self.order.push_head(key);
        self.used_weight += weight;
        let node = Box::new(LruNode {
            data: admit_data(),
            list_index,
            weight,
        });
        self.lookup_table.insert(key, node);
        (0, weight, true)
    }

    /// Update an existing entry in place, or admit it if missing.
    ///
    /// Returns `(old_weight, new_weight)`, where `old_weight` is `None` when a
    /// new entry was inserted.
    pub fn update_or_admit<U, A>(
        &mut self,
        key: u64,
        update: U,
        admit_data: A,
    ) -> (Option<usize>, usize)
    where
        U: FnOnce(&mut T, usize) -> usize,
        A: FnOnce() -> (T, usize),
    {
        if let Some(node) = self.lookup_table.get_mut(&key) {
            let old_weight = node.weight;
            let new_weight = update(&mut node.data, old_weight).max(1);
            let old_weight = Self::adjust_weight(node, &mut self.used_weight, new_weight);
            self.order.promote(node.list_index);
            return (Some(old_weight), new_weight);
        }
        let (data, weight) = admit_data();
        let weight = weight.max(1);
        let list_index = self.order.push_head(key);
        self.used_weight += weight;
        let node = Box::new(LruNode {
            data,
            list_index,
            weight,
        });
        self.lookup_table.insert(key, node);
        (None, weight)
    }

    pub fn access(&mut self, key: u64) -> bool {
        if let Some(node) = self.lookup_table.get(&key) {
            self.order.promote(node.list_index);
            true
        } else {
            false
        }
    }

    /// Set the weight associated with an existing key without changing LRU order.
    pub fn set_weight(&mut self, key: u64, weight: usize) -> Option<usize> {
        let node = self.lookup_table.get_mut(&key)?;
        Some(Self::adjust_weight(node, &mut self.used_weight, weight))
    }

    /// Set the weight when it matches, preserving data and LRU order.
    pub fn set_weight_if_unchanged(
        &mut self,
        key: u64,
        expected_weight: usize,
        weight: usize,
    ) -> bool {
        let Some(node) = self.lookup_table.get_mut(&key) else {
            return false;
        };
        if node.weight != expected_weight {
            return false;
        }
        Self::adjust_weight(node, &mut self.used_weight, weight);
        true
    }

    // Check if a key is already in the top n most recently used nodes.
    // this is a heuristic to reduce write, which requires exclusive locks, for promotion,
    // especially on very populate nodes
    // NOTE: O(n) search here so limit needs to be small
    pub fn need_promote(&self, key: u64, limit: usize) -> bool {
        !self.order.exist_near_head(&key, limit)
    }

    // try to evict 1 node
    pub fn evict(&mut self) -> Option<(T, usize)> {
        self.order.pop_tail().map(|key| {
            // unwrap is safe because we always insert in both the hashtable and the list
            let node = self.lookup_table.remove(&key).unwrap();
            self.used_weight -= node.weight;
            (node.data, node.weight)
        })
    }

    /// Peek at the least-recently-used item without removing it.
    ///
    /// Returns a reference to the data and weight of the tail item, or `None`
    /// if empty.
    pub fn peek_lru(&self) -> Option<(&T, usize)> {
        self.order
            .tail()
            .and_then(|idx| self.order.peek(idx))
            .and_then(|key| self.lookup_table.get(key))
            .map(|node| (&node.data, node.weight))
    }

    // TODO: scan the tail up to K elements to decide which ones to evict

    pub fn remove(&mut self, key: u64) -> Option<(T, usize)> {
        self.lookup_table.remove(&key).map(|node| {
            let list_key = self.order.remove(node.list_index);
            assert_eq!(key, list_key);
            self.used_weight -= node.weight;
            (node.data, node.weight)
        })
    }

    /// Reserve capacity for `additional` more entries in this shard (lookup
    /// table + ordering list), avoiding reallocation/rehashing on bulk load.
    fn reserve(&mut self, additional: usize) {
        self.lookup_table.reserve(additional);
        self.order.reserve(additional);
    }

    pub fn insert_tail(&mut self, key: u64, data: T, weight: usize) -> bool {
        self.insert_at(key, data, weight, Placement::Tail)
    }

    fn insert_at(&mut self, key: u64, data: T, weight: usize, placement: Placement) -> bool {
        if self.lookup_table.contains_key(&key) {
            return false;
        }
        let list_index = match placement {
            Placement::Head => self.order.push_head(key),
            Placement::Tail => self.order.push_tail(key),
        };
        let node = Box::new(LruNode {
            data,
            list_index,
            weight,
        });
        self.lookup_table.insert(key, node);
        self.used_weight += weight;
        true
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        assert_eq!(self.lookup_table.len(), self.order.len());
        self.lookup_table.len()
    }

    #[cfg(test)]
    pub fn used_weight(&self) -> usize {
        self.used_weight
    }

    pub fn iter(&self) -> LruUnitIter<'_, T> {
        LruUnitIter {
            unit: self,
            iter: self.order.iter(),
        }
    }

    // Adjusts node weight to the new given weight.
    // Returns old weight.
    #[inline]
    fn adjust_weight(node: &mut LruNode<T>, used_weight: &mut usize, weight: usize) -> usize {
        let old_weight = node.weight;
        if weight != old_weight {
            *used_weight += weight;
            *used_weight -= old_weight;
            node.weight = weight;
        }
        old_weight
    }
}

struct LruUnitIter<'a, T> {
    unit: &'a LruUnit<T>,
    iter: LinkedListIter<'a, u64>,
}

impl<'a, T> Iterator for LruUnitIter<'a, T> {
    type Item = (&'a T, usize);

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|key| {
            // safe because we always items in table and list are always 1:1
            let node = self.unit.lookup_table.get(key).unwrap();
            (&node.data, node.weight)
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<T> DoubleEndedIterator for LruUnitIter<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back().map(|key| {
            // safe because we always items in table and list are always 1:1
            let node = self.unit.lookup_table.get(key).unwrap();
            (&node.data, node.weight)
        })
    }
}

#[cfg(test)]
mod test_lru {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::{sync::Barrier, thread};

    #[test]
    fn pair_transfer_preserves_weight_counts_and_order() {
        for destination in [2, 3] {
            let lru = Lru::<u64, 2>::with_capacity(100, 0);
            lru.insert_tail(0, 0, 12);
            lru.insert_tail(4, 4, 1);
            lru.mutate_pair(0, destination, |source, target, _| {
                source.as_mut().unwrap().1 -= 5;
                *target = Some((destination, 5));
            });
            assert_eq!(lru.weight(), 13);
            assert_eq!(lru.len(), 3);
            assert_eq!(lru.peek_weight(0), Some(7));
            assert_eq!(lru.peek_weight(destination), Some(5));
            assert_lru(
                &lru,
                if destination == 2 {
                    &[0, 4, 2]
                } else {
                    &[0, 4]
                },
                0,
            );
            lru.mutate_pair(0, destination, |source, target, _| {
                *source = None;
                target.as_mut().unwrap().1 += 7;
            });
            assert_eq!(lru.weight(), 13);
            assert_eq!(lru.len(), 2);
            assert_eq!(lru.shard_len(0) + lru.shard_len(1), 2);
            assert_eq!(lru.peek_weight(destination), Some(12));
            assert!(lru
                .mutate_pair(4, 4, |_, _, _| panic!("same key callback"))
                .is_none());
        }
    }

    #[test]
    fn pair_reverse_lock_order_and_update_are_atomic() {
        let lru = Lru::<u64, 2>::with_capacity(1000, 0);
        lru.insert_tail(0, 0, 100);
        lru.insert_tail(1, 1, 100);
        std::thread::scope(|scope| {
            for (a, b) in [(0, 1), (1, 0)] {
                let lru = &lru;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        lru.mutate_pair(a, b, |left, right, _| {
                            let total = left.as_ref().unwrap().1 + right.as_ref().unwrap().1;
                            assert_eq!(total, 200);
                            std::mem::swap(
                                &mut left.as_mut().unwrap().1,
                                &mut right.as_mut().unwrap().1,
                            );
                        });
                    }
                });
            }
        });
        assert_eq!(lru.weight(), 200);
        assert_eq!(lru.len(), 2);
    }

    #[test]
    fn pair_insert_placement_and_zero_weight_removal() {
        let lru = Lru::<u64, 1>::with_capacity(100, 0);
        lru.insert_tail(1, 1, 0);
        lru.insert_tail(2, 2, 2);
        lru.mutate_pair(1, 3, |source, target, placement| {
            *source = None;
            *target = Some((3, 3));
            *placement = Placement::Head;
        });
        assert_lru(&lru, &[3, 2], 0);
        assert_eq!(lru.peek_weight(1), None);
        assert_accounting(&lru, 5, 2);
        lru.mutate_pair(2, 4, |_, target, _| *target = Some((4, 4)));
        assert_lru(&lru, &[3, 2, 4], 0);
        assert_accounting(&lru, 9, 3);
    }

    #[test]
    fn conditional_weight_removal_rejects_changed_charge() {
        let lru = Lru::<u64, 2>::with_capacity(100, 0);
        lru.insert_tail(0, 7, 10);
        lru.set_weight(0, 12);
        assert!(lru
            .remove_if_weight(0, |data, weight| *data == 7 && weight == 10)
            .is_none());
        assert_eq!(lru.weight(), 12);
        assert_eq!(
            lru.remove_if_weight(0, |data, weight| *data == 7 && weight == 12),
            Some((7, 12))
        );
        assert_eq!(lru.weight(), 0);
        assert_eq!(lru.shard_len(0), 0);
    }

    fn assert_lru<T: Copy + PartialEq + std::fmt::Debug, const N: usize>(
        lru: &Lru<T, N>,
        values: &[T],
        shard: usize,
    ) {
        let mut list_values = vec![];
        lru.iter_for_each(shard, |(v, _)| list_values.push(*v));
        assert_eq!(values, &list_values)
    }

    #[test]
    fn test_reserve_shard() {
        // Start with no pre-sized per-shard capacity.
        let lru = Lru::<u64, 2>::with_capacity(1000, 0);
        assert_eq!(lru.shard_capacity(0), 0);

        lru.reserve_shard(0, 50);
        // The ordering list uses `reserve_exact`, so from empty it is sized
        // exactly to the request with no slack.
        assert_eq!(lru.shard_capacity(0), 50);
        // The lookup table uses `HashMap::reserve`, which guarantees room for at
        // least the requested count but may over-allocate to honor its load
        // factor, so only a lower bound can be asserted.
        assert!(lru.shard_lookup_capacity(0) >= 50);

        // Reserving does not fabricate entries.
        assert_eq!(lru.shard_len(0), 0);

        // keys 0, 2, 4, ... map to shard 0 (key % 2).
        for k in (0..20u64).step_by(2) {
            assert!(lru.insert_tail(k, k, 1));
        }
        assert_eq!(lru.shard_len(0), 10);
        // The 10 inserts fit within the reserved capacity: no reallocation.
        assert_eq!(lru.shard_capacity(0), 50);

        // Order is preserved (inserted at tail, iterated head -> tail).
        assert_lru(&lru, &[0, 2, 4, 6, 8, 10, 12, 14, 16, 18], 0);

        // Out-of-range shard is a no-op, not a panic.
        lru.reserve_shard(99, 100);
    }

    #[test]
    fn test_admit() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        assert_eq!(lru.len(), 0);

        lru.admit(2, 2, 3);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.weight(), 3);

        lru.admit(2, 2, 1);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.weight(), 1);

        lru.admit(2, 2, 2); // admit again with different weight
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.weight(), 2);

        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);

        assert_eq!(lru.weight(), 2 + 3 + 4);
        assert_eq!(lru.len(), 3);
    }

    #[test]
    fn test_promote() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        lru.admit(6, 6, 6);
        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[5, 3], 1);

        assert!(lru.promote(3));
        assert_lru(&lru, &[3, 5], 1);
        assert!(lru.promote(3));
        assert_lru(&lru, &[3, 5], 1);

        assert!(lru.promote(2));
        assert_lru(&lru, &[2, 6, 4], 0);

        assert!(!lru.promote(7)); // 7 doesn't exist
        assert_lru(&lru, &[2, 6, 4], 0);
        assert_lru(&lru, &[3, 5], 1);

        // promote 2 to top 1, already there
        assert!(lru.promote_top_n(2, 1));
        assert_lru(&lru, &[2, 6, 4], 0);

        // promote 4 to top 3, already there
        assert!(lru.promote_top_n(4, 3));
        assert_lru(&lru, &[2, 6, 4], 0);

        // promote 4 to top 2
        assert!(lru.promote_top_n(4, 2));
        assert_lru(&lru, &[4, 2, 6], 0);

        // promote 2 to top 1
        assert!(lru.promote_top_n(2, 1));
        assert_lru(&lru, &[2, 4, 6], 0);

        assert!(!lru.promote_top_n(7, 1)); // 7 doesn't exist
    }

    #[test]
    fn test_evict() {
        let lru = Lru::<_, 2>::with_capacity(14, 10);

        // same weight to make the random eviction less random
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 2);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 4);
        lru.admit(6, 6, 2);
        lru.admit(7, 7, 2);

        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[7, 5, 3], 1);

        assert_eq!(lru.weight(), 16);
        assert_eq!(lru.len(), 6);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 14);
        assert_eq!(lru.len(), 5);
        assert_eq!(lru.evicted_weight(), 2);
        assert_eq!(lru.evicted_len(), 1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].1, 2); //weight
        assert!(evicted[0].0 == 2 || evicted[0].0 == 3); //either 2 or 3 are evicted

        let lru = Lru::<_, 2>::with_capacity(6, 10);

        // same weight random eviction less random
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 2);
        lru.admit(4, 4, 2);
        lru.admit(5, 5, 2);
        lru.admit(6, 6, 2);
        lru.admit(7, 7, 2);
        assert_eq!(lru.weight(), 12);
        assert_eq!(lru.len(), 6);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 6);
        assert_eq!(lru.len(), 3);
        assert_eq!(lru.evicted_weight(), 6);
        assert_eq!(lru.evicted_len(), 3);
        assert_eq!(evicted.len(), 3);
    }

    #[test]
    fn test_increment_weight() {
        let lru = Lru::<_, 2>::with_capacity(6, 10);
        lru.admit(1, 1, 1);
        assert_eq!(lru.increment_weight(1, || 1, 1, None), 2);
        assert_eq!(lru.weight(), 1 + 1);
        assert_eq!(lru.len(), 1);

        assert_eq!(lru.increment_weight(0, || 0, 1000, Some(3)), 3);
        assert_eq!(lru.weight(), 1 + 1 + 3);
        assert_eq!(lru.len(), 2);
        assert_lru(&lru, &[0], 0);

        assert_eq!(lru.increment_weight(4, || 4, 0, None), 1);
        assert_eq!(lru.weight(), 1 + 1 + 3 + 1);
        assert_eq!(lru.len(), 3);

        lru.admit(2, 2, 2);
        assert_eq!(lru.increment_weight(2, || 2, 2, None), 4);
        assert_eq!(lru.weight(), 1 + 1 + 3 + 1 + 2 + 2);

        assert_eq!(lru.increment_weight(2, || 2, 2, Some(3)), 4);
        assert_eq!(lru.weight(), 1 + 1 + 3 + 1 + 4);
    }

    #[test]
    fn test_update_or_admit() {
        let lru = Lru::<_, 1>::with_capacity(30, 10);

        lru.admit(2, 20, 2);
        lru.admit(4, 40, 4);
        assert_lru(&lru, &[40, 20], 0);
        assert_eq!(lru.weight(), 6);
        assert_eq!(lru.len(), 2);

        let old_weight = lru.update_or_admit(
            2,
            |value, weight| {
                *value += 1;
                weight + 3
            },
            || (99, 99),
        );

        assert_eq!(old_weight, Some(2));
        assert_lru(&lru, &[21, 40], 0);
        assert_eq!(lru.weight(), 9);
        assert_eq!(lru.len(), 2);

        let old_weight = lru.update_or_admit(
            6,
            |_value, _weight| unreachable!("missing key should admit"),
            || (60, 0),
        );

        assert_eq!(old_weight, None);
        assert_lru(&lru, &[60, 21, 40], 0);
        assert_eq!(lru.weight(), 10);
        assert_eq!(lru.len(), 3);

        assert_eq!(
            lru.update_or_admit(
                4,
                |value, _weight| {
                    *value += 1;
                    0
                },
                || (99, 99),
            ),
            Some(4)
        );
        assert_lru(&lru, &[41, 60, 21], 0);
        assert_eq!(lru.weight(), 7);
        assert_eq!(lru.len(), 3);
    }

    #[test]
    fn test_set_weight_does_not_promote() {
        let lru = Lru::<_, 1>::with_capacity(30, 10);
        lru.admit(1, 1, 1);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        assert_lru(&lru, &[3, 2, 1], 0);

        assert_eq!(lru.set_weight(2, 5), Some(2));
        assert_eq!(lru.peek_weight(2), Some(5));
        assert_eq!(lru.weight(), 1 + 5 + 3);
        assert_eq!(lru.len(), 3);
        assert_lru(&lru, &[3, 2, 1], 0);

        assert_eq!(lru.set_weight(9, 9), None);
        assert_eq!(lru.weight(), 1 + 5 + 3);
        assert_eq!(lru.len(), 3);
        assert_lru(&lru, &[3, 2, 1], 0);

        assert_eq!(lru.set_weight(2, 0), Some(5));
        assert_eq!(lru.peek_weight(2), Some(1));
        assert_eq!(lru.weight(), 1 + 1 + 3);
        assert_lru(&lru, &[3, 2, 1], 0);
    }

    fn assert_accounting<T, const N: usize>(lru: &Lru<T, N>, weight: usize, len: usize) {
        assert_eq!(lru.weight(), weight);
        assert_eq!(lru.len(), len);
        assert_eq!((lru.evicted_weight(), lru.evicted_len()), (0, 0));
        let mut shard_weight = 0;
        let mut shard_len = 0;
        for shard in 0..N {
            let unit = lru.units[shard].read();
            assert_eq!(lru.shard_len(shard), unit.len());
            shard_weight += unit.used_weight;
            shard_len += unit.len();
        }
        assert_eq!((shard_weight, shard_len), (weight, len));
    }

    #[test]
    fn test_update_if() {
        let lru = Lru::<_, 1>::with_capacity(30, 10);
        lru.admit(1, 10, 2);
        lru.admit(2, 20, 4);
        assert_eq!(
            lru.update_if(
                3,
                |_| unreachable!("absent key"),
                |_, _| unreachable!("absent key"),
                true,
            ),
            None
        );
        assert_eq!(
            lru.update_if(
                1,
                |data| *data == 99,
                |_, _| unreachable!("rejected key"),
                true,
            ),
            None
        );
        assert_lru(&lru, &[20, 10], 0);
        assert_eq!(lru.peek_weight(1), Some(2));
        assert_accounting(&lru, 6, 2);

        assert_eq!(
            lru.update_if(
                1,
                |data| *data == 10,
                |data, weight| {
                    *data += 1;
                    weight + 3
                },
                false,
            ),
            Some(2)
        );
        assert_lru(&lru, &[20, 11], 0);
        assert_eq!(lru.peek_weight(1), Some(5));
        assert_accounting(&lru, 9, 2);

        assert_eq!(lru.update_if(1, |_| true, |_, _| 0, true), Some(5));
        assert_lru(&lru, &[11, 20], 0);
        assert_eq!(lru.peek_weight(1), Some(1));
        assert_accounting(&lru, 5, 2);
        assert_eq!(lru.update_if(2, |_| true, |_, w| w, true), Some(4));
        assert_lru(&lru, &[20, 11], 0);
        assert_accounting(&lru, 5, 2);
    }

    #[test]
    fn test_upsert_if() {
        let lru = Lru::<_, 1>::with_capacity(30, 10);
        assert_eq!(
            lru.upsert_if(
                1,
                |_| unreachable!("absent key"),
                |_, _| unreachable!("absent key"),
                || (10, 0),
            ),
            Upsert::Admitted
        );
        lru.admit(2, 20, 4);
        assert_accounting(&lru, 5, 2);
        assert_eq!(
            lru.upsert_if(
                1,
                |data| *data == 99,
                |_, _| unreachable!("colliding identity"),
                || unreachable!("existing key"),
            ),
            Upsert::Rejected
        );
        assert_lru(&lru, &[20, 10], 0);
        assert_eq!(lru.peek_weight(1), Some(1));
        assert_accounting(&lru, 5, 2);

        assert_eq!(
            lru.upsert_if(
                1,
                |data| *data == 10,
                |data, weight| {
                    *data += 1;
                    weight + 5
                },
                || unreachable!("existing key"),
            ),
            Upsert::Updated(1)
        );
        assert_lru(&lru, &[11, 20], 0);
        assert_accounting(&lru, 10, 2);
        assert_eq!(
            lru.upsert_if(2, |_| true, |_, _| 0, || unreachable!("existing key")),
            Upsert::Updated(4)
        );
        assert_lru(&lru, &[20, 11], 0);
        assert_eq!(lru.peek_weight(2), Some(1));
        assert_accounting(&lru, 7, 2);
        assert_eq!(
            lru.upsert_if(1, |_| true, |_, w| w, || unreachable!("existing key")),
            Upsert::Updated(6)
        );
        assert_lru(&lru, &[11, 20], 0);
        assert_accounting(&lru, 7, 2);
    }

    #[test]
    fn test_remove_if() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 20, 2);
        lru.admit(4, 40, 4);
        lru.admit(1, 10, 3);
        assert_eq!(lru.remove_if(6, |_| unreachable!("absent key")), None);
        assert_eq!(lru.remove_if(2, |data| *data == 99), None);
        assert_lru(&lru, &[40, 20], 0);
        assert_lru(&lru, &[10], 1);
        assert_eq!(lru.peek_weight(2), Some(2));
        assert_accounting(&lru, 9, 3);
        assert_eq!(lru.remove_if(2, |data| *data == 20), Some((20, 2)));
        assert_lru(&lru, &[40], 0);
        assert_accounting(&lru, 7, 2);
        assert_eq!(lru.remove_if(2, |_| unreachable!("removed key")), None);
        assert_accounting(&lru, 7, 2);
    }

    #[test]
    fn test_evict_if() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 20, 2);
        lru.admit(4, 40, 4);
        lru.admit(1, 10, 3);
        assert_eq!(lru.evict_if(6, |_| unreachable!("absent key")), None);
        assert_eq!(lru.evict_if(2, |data| *data == 99), None);
        assert_accounting(&lru, 9, 3);

        // Selection alone must not count as an eviction.
        let selected = lru.select_lru_if(0, 2, |_| true, |_, _| {});
        assert_eq!(selected, Some((20, 2)));
        assert_accounting(&lru, 9, 3);

        assert_eq!(lru.evict_if(2, |data| *data == 20), Some((20, 2)));
        assert_eq!((lru.weight(), lru.len()), (7, 2));
        assert_eq!((lru.evicted_weight(), lru.evicted_len()), (2, 1));
        assert_lru(&lru, &[40], 0);
        assert_lru(&lru, &[10], 1);
        assert_eq!(lru.evict_if(2, |_| unreachable!("evicted key")), None);

        // Explicit removal still leaves the eviction counters alone.
        assert_eq!(lru.remove_if(4, |_| true), Some((40, 4)));
        assert_eq!(lru.evict_if(1, |data| *data == 10), Some((10, 3)));
        assert_eq!((lru.weight(), lru.len()), (0, 0));
        assert_eq!((lru.evicted_weight(), lru.evicted_len()), (5, 2));
    }

    #[test]
    fn test_select_lru_if() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        assert_eq!(
            lru.select_lru_if(0, 3, |_| true, |_, _| unreachable!("empty")),
            None
        );
        lru.admit(2, 20, 2);
        lru.admit(4, 40, 4);
        lru.admit(6, 60, 6);
        lru.admit(1, 10, 1);
        assert_eq!(
            lru.select_lru_if(2, 3, |_| false, |_, _| unreachable!("no eligible entry")),
            None
        );
        assert_lru(&lru, &[60, 40, 20], 0);
        assert_accounting(&lru, 13, 4);
        assert_eq!(
            lru.select_lru_if(
                2,
                3,
                |data| *data >= 40,
                |data, weight| {
                    assert_eq!(weight, 4);
                    *data += 1;
                }
            ),
            Some((41, 4))
        );
        assert_lru(&lru, &[41, 20, 60], 0);
        assert_lru(&lru, &[10], 1);
        assert_eq!(lru.peek_weight(4), Some(4));
        assert_accounting(&lru, 13, 4);
        assert_eq!(
            lru.select_lru_if(
                0,
                3,
                |data| *data == 20,
                |data, weight| {
                    assert_eq!(weight, 2);
                    *data += 1;
                }
            ),
            Some((21, 2))
        );
        assert_lru(&lru, &[21, 60, 41], 0);
        assert_accounting(&lru, 13, 4);
    }

    #[test]
    fn bounded_selection_rotates_skipped_entries() {
        let lru = Lru::<u64, 1>::with_capacity(100, 0);
        lru.insert_tail(1, 1, 7);
        for key in 2..=6 {
            lru.insert_tail(key, key, 1);
        }
        assert_eq!(
            lru.select_lru_if(0, 0, |_| true, |_, _| unreachable!()),
            None
        );
        assert_eq!(
            lru.select_lru_if(0, 2, |data| *data == 1, |_, _| unreachable!()),
            None
        );
        assert_accounting(&lru, 12, 6);
        assert_eq!(
            lru.select_lru_if(0, 2, |data| *data == 1, |_, _| unreachable!()),
            None
        );
        assert_accounting(&lru, 12, 6);
        assert_eq!(
            lru.select_lru_if(0, 2, |data| *data == 1, |_, w| assert_eq!(w, 7)),
            Some((1, 7))
        );
        assert_accounting(&lru, 12, 6);
        assert_eq!(
            lru.select_lru_if(0, 6, |_| false, |_, _| unreachable!()),
            None
        );
        assert_accounting(&lru, 12, 6);
    }

    #[test]
    fn test_update_remove_if_race() {
        use std::sync::Barrier;
        use std::thread;

        let lru = Lru::<_, 1>::with_capacity(30, 10);
        lru.admit(1, 10, 2);
        let held = Barrier::new(2);
        let release = Barrier::new(2);
        let contender = Barrier::new(2);
        thread::scope(|scope| {
            let update = scope.spawn(|| {
                lru.update_if(
                    1,
                    |data| {
                        held.wait();
                        release.wait();
                        *data == 10
                    },
                    |data, weight| {
                        *data = 20;
                        weight + 3
                    },
                    true,
                )
            });
            held.wait();
            let locked = lru.units[0].try_write().is_none();
            let remove = scope.spawn(|| {
                contender.wait();
                lru.remove_if(1, |data| *data == 20)
            });
            contender.wait();
            release.wait();
            assert_eq!(update.join().unwrap(), Some(2));
            assert_eq!(remove.join().unwrap(), Some((20, 5)));
            assert!(locked);
        });
        assert_accounting(&lru, 0, 0);

        lru.admit(1, 10, 2);
        thread::scope(|scope| {
            let remove = scope.spawn(|| {
                lru.remove_if(1, |data| {
                    held.wait();
                    release.wait();
                    *data == 10
                })
            });
            held.wait();
            let locked = lru.units[0].try_write().is_none();
            let update = scope.spawn(|| {
                contender.wait();
                lru.update_if(
                    1,
                    |_| unreachable!("removed key"),
                    |_, _| unreachable!("removed key"),
                    true,
                )
            });
            contender.wait();
            release.wait();
            assert_eq!(remove.join().unwrap(), Some((10, 2)));
            assert_eq!(update.join().unwrap(), None);
            assert!(locked);
        });
        assert_lru(&lru, &[], 0);
        assert_accounting(&lru, 0, 0);
    }

    #[test]
    fn test_set_weight_if_unchanged_preserves_entries() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 20, 2);
        lru.admit(4, 40, 4);
        lru.admit(6, 60, 6);
        lru.admit(1, 10, 10);

        for (expected_weight, weight, stored_weight) in [(4, 9, 9), (9, 3, 3), (3, 0, 1), (1, 1, 1)]
        {
            assert!(lru.set_weight_if_unchanged(4, expected_weight, weight));
            assert_eq!(lru.peek_weight(4), Some(stored_weight));
            assert_eq!(lru.shard_weight(0), 2 + stored_weight + 6);
            assert_eq!(lru.shard_weight(1), 10);
            assert_eq!(lru.weight(), 2 + stored_weight + 6 + 10);
            assert_eq!(lru.len(), 4);
            assert_eq!(lru.shard_len(0), 3);
            assert_eq!(lru.shard_len(1), 1);
            assert_eq!(lru.evicted_len(), 0);
            assert_eq!(lru.evicted_weight(), 0);
            assert_lru(&lru, &[60, 40, 20], 0);
            assert_lru(&lru, &[10], 1);
        }

        assert!(!lru.set_weight_if_unchanged(4, 0, 5));
        assert_eq!(lru.peek_weight(4), Some(1));
        assert_eq!(lru.weight(), 19);
    }

    #[test]
    fn test_set_weight_if_unchanged_rejects_stale_weight() {
        let lru = Lru::<_, 1>::with_capacity(30, 10);
        lru.admit(2, 20, 2);
        lru.admit(4, 40, 4);
        let observed_weight = lru.peek_weight(4).unwrap();
        assert_eq!(lru.increment_weight(4, || 99, 5, None), 9);
        lru.admit(6, 60, 6);

        assert!(!lru.set_weight_if_unchanged(4, observed_weight, 1));
        assert_eq!(lru.peek_weight(4), Some(9));
        assert_eq!(lru.shard_weight(0), 17);
        assert_eq!(lru.weight(), 17);
        assert_eq!(lru.len(), 3);
        assert_eq!(lru.shard_len(0), 3);
        assert_lru(&lru, &[60, 40, 20], 0);
    }

    #[test]
    fn test_set_weight_if_unchanged_missing_key() {
        let lru = Lru::<_, 1>::with_capacity(30, 10);
        lru.admit(1, 10, 3);

        assert!(!lru.set_weight_if_unchanged(2, 0, 9));
        assert_eq!(lru.peek_weight(2), None);
        assert_eq!(lru.shard_weight(0), 3);
        assert_eq!(lru.weight(), 3);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.shard_len(0), 1);
        assert_lru(&lru, &[10], 0);
    }

    #[test]
    fn test_set_weight_if_unchanged_concurrent_increment() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(1, 10, 5);
        lru.admit(2, 20, 8);

        for _ in 0..32 {
            lru.set_weight(2, 8);
            let start = Barrier::new(2);
            let updated = thread::scope(|scope| {
                let setter = scope.spawn(|| {
                    start.wait();
                    lru.set_weight_if_unchanged(2, 8, 2)
                });
                let incrementer = scope.spawn(|| {
                    start.wait();
                    lru.increment_weight(2, || 99, 4, None);
                });
                let updated = setter.join().unwrap();
                incrementer.join().unwrap();
                updated
            });

            let expected_weight = if updated { 6 } else { 12 };
            assert_eq!(lru.peek_weight(2), Some(expected_weight));
            assert_eq!(lru.shard_weight(0), expected_weight);
            assert_eq!(lru.weight(), expected_weight + 5);
            assert_eq!(lru.len(), 2);
            assert_eq!(lru.shard_len(0), 1);
            assert_eq!(lru.shard_len(1), 1);
            assert_lru(&lru, &[20], 0);
            assert_lru(&lru, &[10], 1);
        }
    }

    /// Every path that changes an existing entry's weight (`admit` replacing an
    /// entry, `update_or_admit`, `set_weight` and `set_weight_if_unchanged`) must
    /// apply the change to the global weight as one net-delta atomic update, so
    /// that `weight()` never reports a total that no entry state ever had.
    ///
    /// The LRU holds a single entry whose weight toggles between `LO` and `HI`, so
    /// any other sampled value is such a transient, and correct code cannot fail
    /// the check. A split update is caught only when a reader samples inside the
    /// few instructions between its two halves, which cannot be widened without
    /// instrumenting the code under test, so detection is probabilistic: each of
    /// the `ITERS` updates opens a new window, and `READERS` threads sample
    /// `weight()` continuously while the writer runs.
    #[test]
    fn weight_shadow_never_transiently_exceeds_the_true_value() {
        #[derive(Debug, Clone, Copy)]
        enum Site {
            SetWeight,
            SetWeightIfUnchanged,
            UpdateOrAdmit,
            AdmitReplace,
        }

        struct StopOnDrop<'a>(&'a AtomicBool);

        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        const LO: usize = 3;
        const HI: usize = 1_000_003;
        const ITERS: usize = 50_000;
        const READERS: usize = 2;

        for site in [
            Site::SetWeight,
            Site::SetWeightIfUnchanged,
            Site::UpdateOrAdmit,
            Site::AdmitReplace,
        ] {
            let lru = Lru::<i32, 1>::with_capacity(usize::MAX / 2, 4);
            lru.admit(1, 0, LO);

            let violation = AtomicBool::new(false);
            let stop = AtomicBool::new(false);
            // Releases the readers and the writer together, so every reader
            // thread has started before the writer mutates. Without it, a loaded
            // or single-core scheduler could let the writer finish every
            // iteration before a reader thread ever runs, silently turning this
            // into a no-op check. The barrier makes overlap likely, not certain.
            let start = Barrier::new(READERS + 1);

            thread::scope(|scope| {
                for _ in 0..READERS {
                    let lru = &lru;
                    let violation = &violation;
                    let stop = &stop;
                    let start = &start;
                    scope.spawn(move || {
                        start.wait();
                        while !stop.load(Ordering::Relaxed) {
                            let w = lru.weight();
                            if w != LO && w != HI {
                                violation.store(true, Ordering::Relaxed);
                            }
                        }
                    });
                }
                start.wait();
                // Readers spin until `stop` is set, and `thread::scope` joins them
                // before re-raising a panic, so a failed check below must set it too.
                let _stop = StopOnDrop(&stop);

                let mut current = LO;
                for _ in 0..ITERS {
                    let next = if current == LO { HI } else { LO };
                    match site {
                        Site::SetWeight => {
                            lru.set_weight(1, next);
                        }
                        Site::SetWeightIfUnchanged => {
                            assert!(lru.set_weight_if_unchanged(1, current, next));
                        }
                        Site::UpdateOrAdmit => {
                            lru.update_or_admit(1, |_, _| next, || unreachable!());
                        }
                        Site::AdmitReplace => {
                            lru.admit(1, 0, next);
                        }
                    }
                    current = next;
                }
            });

            assert!(
                !violation.load(Ordering::Relaxed),
                "{site:?}: weight() observed a value other than {LO} or {HI}"
            );
        }
    }

    #[test]
    fn test_remove() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        lru.admit(6, 6, 6);

        assert_eq!(lru.weight(), 2 + 3 + 4 + 5 + 6);
        assert_eq!(lru.len(), 5);
        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[5, 3], 1);

        let node = lru.remove(6).unwrap();
        assert_eq!(node.0, 6); // data
        assert_eq!(node.1, 6); // weight
        assert_eq!(lru.weight(), 2 + 3 + 4 + 5);
        assert_eq!(lru.len(), 4);
        assert_lru(&lru, &[4, 2], 0);

        let node = lru.remove(3).unwrap();
        assert_eq!(node.0, 3); // data
        assert_eq!(node.1, 3); // weight
        assert_eq!(lru.weight(), 2 + 4 + 5);
        assert_eq!(lru.len(), 3);
        assert_lru(&lru, &[5], 1);

        assert!(lru.remove(7).is_none());
    }

    #[test]
    fn test_peek() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);

        assert!(lru.peek(4));
        assert!(lru.peek(3));
        assert!(lru.peek(2));

        assert_lru(&lru, &[4, 2], 0);
        assert_lru(&lru, &[3], 1);
    }

    #[test]
    fn test_peek_with() {
        let lru = Lru::<String, 1>::with_capacity(10, 10);
        lru.admit(2, "value".to_owned(), 1);

        assert!(lru.peek_with(2, |data| data.is_some_and(|value| value == "value")));
        assert!(!lru.peek_with(3, |data| data.is_some()));
    }

    #[test]
    fn test_insert_tail() {
        let lru = Lru::<_, 2>::with_capacity(30, 10);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        lru.admit(6, 6, 6);

        assert_eq!(lru.weight(), 2 + 3 + 4 + 5 + 6);
        assert_eq!(lru.len(), 5);
        assert_lru(&lru, &[6, 4, 2], 0);
        assert_lru(&lru, &[5, 3], 1);

        assert!(lru.insert_tail(7, 7, 7));
        assert_eq!(lru.weight(), 2 + 3 + 4 + 5 + 6 + 7);
        assert_eq!(lru.len(), 6);
        assert_lru(&lru, &[5, 3, 7], 1);

        // ignore existing ones
        assert!(!lru.insert_tail(6, 6, 7));
    }

    #[test]
    fn test_evict_to_limit_p2c_bias() {
        use rand::rngs::StdRng;
        use rand::SeedableRng;

        // Shard 0 starts with 50 items, shard 1 with 10 (all weight 1).
        // weight_limit=30 forces 30 evictions. P2C-by-length should pick
        // shard 0 (the longer one) most of the time, driving toward
        // balance. Expected share from shard 0: P2C ≈ 0.75 (P(shard 0)
        // = 3/4 per pick while it stays longer), uniform ≈ 0.50,
        // always-shortest ≈ 0.67 (capped by shard 1's 10 items),
        // always-longest ≈ 1.0. The (0.65..0.95) window distinguishes
        // P2C from uniform; the upper bound catches a degenerate
        // always-longest regression.
        const TRIALS: u64 = 50;
        let mut total_from_shard0 = 0usize;
        let mut total_evicted = 0usize;

        for seed in 0..TRIALS {
            let lru = Lru::<u64, 2>::with_capacity(30, 64);
            for k in 0..50u64 {
                // even keys → shard 0
                lru.admit(k * 2, k * 2, 1);
            }
            for k in 0..10u64 {
                // odd keys → shard 1
                lru.admit(k * 2 + 1, k * 2 + 1, 1);
            }
            assert_eq!(lru.weight(), 60);

            let mut rng = StdRng::seed_from_u64(seed);
            let evicted = lru.evict_to_limit_with_rng(&mut rng);
            assert!(
                lru.weight() <= 30,
                "post-eviction weight {} exceeds limit",
                lru.weight()
            );
            total_from_shard0 += evicted.iter().filter(|(k, _)| k % 2 == 0).count();
            total_evicted += evicted.len();
        }

        assert!(total_evicted > 1000, "too few evictions: {total_evicted}");
        let share = total_from_shard0 as f64 / total_evicted as f64;
        assert!(
            (0.65..0.95).contains(&share),
            "expected shard-0 eviction share in 0.65..0.95 (P2C ≈ 0.75); got {share}"
        );
    }

    #[test]
    fn test_evict_to_limit_break_on_empty_shards_over_limit() {
        // Force `weight` above the limit while every shard is empty
        // (simulating bookkeeping skew). The linear probe must wrap
        // around all N shards and exit cleanly.
        let lru = Lru::<u64, 4>::with_capacity(10, 16);
        lru.weight.fetch_add(100, Ordering::Relaxed);
        assert_eq!(lru.evict_to_limit().len(), 0);
    }

    #[test]
    fn test_watermark_eviction_with_zero_weight_items() {
        // All items have weight 0 so the weight-limit guard never fires;
        // only the length watermark drives eviction. P2C-by-length should
        // still reach the watermark regardless of weight values.
        let lru = Lru::<u64, 2>::with_capacity_and_watermark(usize::MAX / 2, 10, Some(2));
        for k in 0..6u64 {
            lru.insert_tail(k, k, 0);
        }
        assert_eq!(lru.len(), 6);
        assert_eq!(lru.weight(), 0);
        let evicted = lru.evict_to_limit();
        assert_eq!(lru.len(), 2);
        assert_eq!(evicted.len(), 4);
    }

    #[test]
    fn test_evict_to_limit_with_mostly_empty_shards() {
        // 7/8 shards empty: both random rolls land on empty shards ~77%
        // of the time, exercising the linear-probe fallback heavily.
        let lru = Lru::<u64, 8>::with_capacity(2, 16);
        for k in 0..8u64 {
            // multiples of 8 hash to shard 0
            lru.admit(k * 8, k * 8, 1);
        }
        assert_eq!(lru.weight(), 8);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 2);
        assert_eq!(evicted.len(), 6);
        assert!(evicted.iter().all(|(k, _)| k % 8 == 0));
    }

    #[test]
    fn test_evict_to_limit_below_limit_returns_immediately() {
        // Smoke test: outer guard short-circuits when already under limit.
        let lru = Lru::<u64, 4>::with_capacity(0, 16);
        assert_eq!(lru.evict_to_limit().len(), 0);
    }

    #[test]
    fn test_evict_to_limit_n1() {
        // N=1 is a trivial special case in the selection logic; ensure
        // basic eviction still works.
        let lru = Lru::<u64, 1>::with_capacity(2, 16);
        for k in 0..5u64 {
            lru.admit(k, k, 1);
        }
        assert_eq!(lru.weight(), 5);
        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 2);
        assert_eq!(evicted.len(), 3);
    }

    #[test]
    fn test_set_weight_limit_affects_eviction() {
        let lru = Lru::<u64, 1>::with_capacity(10, 16);
        for k in 0..5u64 {
            lru.admit(k, k, 2);
        }
        assert_eq!(lru.weight(), 10);
        assert_eq!(lru.weight_limit(), 10);

        lru.set_weight_limit(4);
        assert_eq!(lru.weight_limit(), 4);
        let evicted = lru.evict_to_limit();
        assert_eq!(lru.weight(), 4);
        assert_eq!(evicted.len(), 3);

        lru.set_weight_limit(20);
        assert_eq!(lru.evict_to_limit().len(), 0);
    }

    #[test]
    fn test_watermark_eviction() {
        const WEIGHT_LIMIT: usize = usize::MAX / 2;
        let lru = Lru::<u64, 2>::with_capacity_and_watermark(WEIGHT_LIMIT, 10, Some(4));

        // admit 6 items, each weight 1
        for k in [2u64, 3, 4, 5, 6, 7] {
            lru.admit(k, k, 1);
        }

        assert!(lru.weight() < WEIGHT_LIMIT);
        assert_eq!(lru.len(), 6);

        let evicted = lru.evict_to_limit();
        assert_eq!(lru.len(), 4);
        assert_eq!(evicted.len(), 2);
        assert_eq!(lru.evicted_len(), 2);
    }

    #[test]
    fn test_peek_lru() {
        let lru = Lru::<u32, 1>::with_capacity(10, 10);

        // empty shard
        assert!(lru.peek_lru(0).is_none());

        lru.admit(1, 10, 1);
        assert_eq!(lru.peek_lru(0).unwrap(), (10, 1));

        lru.admit(2, 20, 2);
        // key 1 is LRU tail
        assert_eq!(lru.peek_lru(0).unwrap(), (10, 1));

        // promote key 1
        lru.promote(1);
        // key 2 is now LRU tail
        assert_eq!(lru.peek_lru(0).unwrap(), (20, 2));

        // out-of-bounds returns None
        assert!(lru.peek_lru(999).is_none());
    }
}

#[cfg(test)]
mod test_lru_unit {
    use super::*;

    fn assert_lru<T: Copy + PartialEq + std::fmt::Debug>(lru: &LruUnit<T>, values: &[T]) {
        let list_values: Vec<_> = lru.iter().map(|(v, _)| *v).collect();
        assert_eq!(values, &list_values)
    }

    #[test]
    fn test_admit() {
        let mut lru = LruUnit::with_capacity(10);
        assert_eq!(lru.len(), 0);
        assert!(lru.peek(0).is_none());

        lru.admit(2, 2, 1);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.peek(2).unwrap(), &2);
        assert_eq!(lru.used_weight(), 1);

        lru.admit(2, 2, 2); // admit again with different weight
        assert_eq!(lru.used_weight(), 2);

        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);

        assert_eq!(lru.used_weight(), 2 + 3 + 4);
        assert_lru(&lru, &[4, 3, 2]);
    }

    #[test]
    fn test_access() {
        let mut lru = LruUnit::with_capacity(10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        assert_lru(&lru, &[4, 3, 2]);

        assert!(lru.access(3));
        assert_lru(&lru, &[3, 4, 2]);
        assert!(lru.access(3));
        assert_lru(&lru, &[3, 4, 2]);
        assert!(lru.access(2));
        assert_lru(&lru, &[2, 3, 4]);

        assert!(!lru.access(5)); // 5 doesn't exist
        assert_lru(&lru, &[2, 3, 4]);

        assert!(!lru.need_promote(2, 1));
        assert!(lru.need_promote(3, 1));
        assert!(!lru.need_promote(4, 9999));
    }

    #[test]
    fn test_evict() {
        let mut lru = LruUnit::with_capacity(10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        assert_lru(&lru, &[4, 3, 2]);

        assert!(lru.access(3));
        assert!(lru.access(3));
        assert!(lru.access(2));
        assert_lru(&lru, &[2, 3, 4]);

        assert_eq!(lru.used_weight(), 2 + 3 + 4);
        assert_eq!(lru.evict(), Some((4, 4)));
        assert_eq!(lru.used_weight(), 2 + 3);
        assert_lru(&lru, &[2, 3]);

        assert_eq!(lru.evict(), Some((3, 3)));
        assert_eq!(lru.used_weight(), 2);
        assert_lru(&lru, &[2]);

        assert_eq!(lru.evict(), Some((2, 2)));
        assert_eq!(lru.used_weight(), 0);
        assert_lru(&lru, &[]);

        assert_eq!(lru.evict(), None);
        assert_eq!(lru.used_weight(), 0);
        assert_lru(&lru, &[]);
    }

    #[test]
    fn test_increment_weight() {
        let mut lru = LruUnit::with_capacity(10);
        lru.admit(1, 1, 1);
        assert_eq!(lru.increment_weight(1, || 1, 1, None), (1, 2, false));
        assert_eq!(lru.used_weight(), 1 + 1);

        assert_eq!(lru.increment_weight(0, || 0, 1000, Some(3)), (0, 3, true));
        assert_eq!(lru.used_weight(), 1 + 1 + 3);
        assert_lru(&lru, &[0, 1]);

        assert_eq!(lru.increment_weight(4, || 4, 0, None), (0, 1, true));
        assert_eq!(lru.used_weight(), 1 + 1 + 3 + 1);

        lru.admit(2, 2, 2);
        assert_eq!(lru.increment_weight(2, || 2, 2, None), (2, 4, false));
        assert_eq!(lru.used_weight(), 1 + 1 + 3 + 1 + 2 + 2);

        lru.admit(3, 3, 3);
        assert_eq!(lru.increment_weight(3, || 3, 3, Some(5)), (3, 5, false));
        assert_eq!(lru.used_weight(), 1 + 1 + 3 + 1 + 2 + 2 + 3 + 2);

        assert_eq!(lru.increment_weight(3, || 3, 3, Some(3)), (5, 5, false));
        assert_eq!(lru.used_weight(), 1 + 1 + 3 + 1 + 2 + 2 + 3 + 2);
    }

    #[test]
    fn test_set_weight_does_not_promote() {
        let mut lru = LruUnit::with_capacity(10);
        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        assert_lru(&lru, &[4, 3, 2]);

        assert_eq!(lru.set_weight(3, 6), Some(3));
        assert_eq!(lru.peek_weight(3), Some(6));
        assert_eq!(lru.used_weight(), 2 + 6 + 4);
        assert_lru(&lru, &[4, 3, 2]);

        assert_eq!(lru.set_weight(5, 5), None);
        assert_eq!(lru.used_weight(), 2 + 6 + 4);
        assert_lru(&lru, &[4, 3, 2]);
    }

    #[test]
    fn test_remove() {
        let mut lru = LruUnit::with_capacity(10);

        lru.admit(2, 2, 2);
        lru.admit(3, 3, 3);
        lru.admit(4, 4, 4);
        lru.admit(5, 5, 5);
        assert_lru(&lru, &[5, 4, 3, 2]);

        assert!(lru.access(4));
        assert!(lru.access(3));
        assert!(lru.access(3));
        assert!(lru.access(2));
        assert_lru(&lru, &[2, 3, 4, 5]);

        assert_eq!(lru.used_weight(), 2 + 3 + 4 + 5);
        assert_eq!(lru.remove(2), Some((2, 2)));
        assert_eq!(lru.used_weight(), 3 + 4 + 5);
        assert_lru(&lru, &[3, 4, 5]);

        assert_eq!(lru.remove(4), Some((4, 4)));
        assert_eq!(lru.used_weight(), 3 + 5);
        assert_lru(&lru, &[3, 5]);

        assert_eq!(lru.remove(5), Some((5, 5)));
        assert_eq!(lru.used_weight(), 3);
        assert_lru(&lru, &[3]);

        assert_eq!(lru.remove(1), None);
        assert_eq!(lru.used_weight(), 3);
        assert_lru(&lru, &[3]);

        assert_eq!(lru.remove(3), Some((3, 3)));
        assert_eq!(lru.used_weight(), 0);
        assert_lru(&lru, &[]);
    }

    #[test]
    fn test_insert_tail() {
        let mut lru = LruUnit::with_capacity(10);
        assert_eq!(lru.len(), 0);
        assert!(lru.peek(0).is_none());

        assert!(lru.insert_tail(2, 2, 1));
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.peek(2).unwrap(), &2);
        assert_eq!(lru.used_weight(), 1);

        assert!(!lru.insert_tail(2, 2, 2));
        assert!(lru.insert_tail(3, 3, 3));
        assert_eq!(lru.used_weight(), 1 + 3);
        assert_lru(&lru, &[2, 3]);

        assert!(lru.insert_tail(4, 4, 4));
        assert!(lru.insert_tail(5, 5, 5));
        assert_eq!(lru.used_weight(), 1 + 3 + 4 + 5);
        assert_lru(&lru, &[2, 3, 4, 5]);
    }

    #[test]
    fn test_peek_lru() {
        let mut lru = LruUnit::with_capacity(10);

        // empty returns None
        assert!(lru.peek_lru().is_none());

        // single item is both head and tail
        lru.admit(1, 10, 1);
        let (data, weight) = lru.peek_lru().unwrap();
        assert_eq!(*data, 10);
        assert_eq!(weight, 1);

        // second admission pushes first to tail
        lru.admit(2, 20, 2);
        let (data, _) = lru.peek_lru().unwrap();
        assert_eq!(*data, 10); // key 1 is LRU tail

        // promote key 1 — now key 2 is tail
        lru.access(1);
        let (data, _) = lru.peek_lru().unwrap();
        assert_eq!(*data, 20); // key 2 is now LRU tail

        // peek doesn't remove
        assert!(lru.peek_lru().is_some());
        assert!(lru.peek(1).is_some());
        assert!(lru.peek(2).is_some());
    }
}
