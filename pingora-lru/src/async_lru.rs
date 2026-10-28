//! An LRU that separates the read path (lock-free) from the ordering path
//! (actor-based).
//!
//! # Design
//!
//! - **Read path**: A lock-free [`flurry::HashMap`] stores `u64_hash → weight`
//!   for instant `peek` / `peek_weight` lookups with zero contention.
//!
//! - **Ordering path**: An internal tokio task per shard owns the LRU
//!   linked list. Mutations are sent via unbounded channels. The actor is
//!   the **sole writer** to the flurry map.
//!
//! - **Single-copy keys**: The real key `K` lives only in the linked list
//!   arena nodes. The actor maintains a lightweight `u64 → list_index`
//!   hashmap for O(1) promote/lookup. No key duplication.
//!
//! - **Eviction**: Configurable eviction workers watch a
//!   [`tokio::sync::watch`] channel. When any shard actor observes that
//!   the global weight exceeds the limit, it signals the watch. Each
//!   eviction worker wakes up, picks a shard via P2C, and sends an
//!   `Evict` message through the shard's normal channel. The shard
//!   pops its LRU tail, updates counters, and invokes the eviction
//!   callback. Each worker sends one `Evict` then yields before
//!   re-checking the limit, pacing eviction without blocking.
//!
//! - **Serialization**: Pipelined via the actor — no locks, no contention.

use crate::linked_list::LinkedList;
use hashbrown::HashMap;
use rand::{Rng, SeedableRng};
use std::future::Future;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot, watch};

/// Hash a key to u64 using the fixed default ahash seed.
///
/// Uses `AHasher::default()` which has a deterministic compile-time
/// seed. This ensures consistent shard assignment across restarts and
/// compatibility with the sync `Lru` which uses the same hasher.
///
/// Public so that callers (e.g. the eviction manager) can compute the
/// same hash for shard routing.
pub fn hash_key<K: Hash>(key: &K) -> u64 {
    use std::hash::Hasher;
    let mut hasher = ahash::AHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

/// Enqueue a message to a shard actor.
///
/// The shard channels are unbounded, so this never blocks and never
/// drops: `send` fails only if the receiver (shard actor) is gone, which
/// happens only during shutdown. The trade-off for never dropping is that
/// a shard whose actor cannot keep up grows its queue without bound —
/// there is no backpressure.
fn send_msg<K: Send>(tx: &mpsc::UnboundedSender<LruMsg<K>>, msg: LruMsg<K>) {
    let _ = tx.send(msg);
}

fn get_shard<K: Hash>(key: &K, n_shards: usize) -> usize {
    get_shard_from_hash(hash_key(key), n_shards)
}

fn get_shard_from_hash(key_hash: u64, n_shards: usize) -> usize {
    (key_hash % n_shards as u64) as usize
}

/// A message sent to a shard actor.
enum LruMsg<K: Send> {
    Admit {
        key: K,
        weight: usize,
    },
    /// Increment a key's weight by `delta` (capped at `max_weight`), admitting
    /// the key if it is not already present.
    IncrementWeight {
        key: K,
        delta: usize,
        max_weight: Option<usize>,
    },
    Promote {
        key_hash: u64,
    },
    Remove {
        key_hash: u64,
    },
    InsertTail {
        key: K,
        weight: usize,
    },
    /// Evict one item from this shard's LRU tail. Sent by the eviction
    /// actor. The shard updates counters and calls the eviction callback.
    Evict,
    PeekLru {
        resp: oneshot::Sender<Option<(K, usize)>>,
    },
    Snapshot {
        resp: oneshot::Sender<Vec<(K, usize)>>,
    },
    QueryWeight {
        resp: oneshot::Sender<usize>,
    },
}

/// Serialized shard data produced by [`AsyncLru::save`] for a single shard.
///
/// Passed to the user-provided `write_shard` callback so that it can persist
/// each shard independently (e.g. to a separate file).
pub struct ShardData<K> {
    /// Zero-based index of the shard this data came from.
    pub shard_index: usize,
    /// Items in LRU order (most-recently-used first), each paired with its weight.
    pub items: Vec<(K, usize)>,
}

/// Per-shard state owned exclusively by the actor task.
///
/// Each linked list node stores `(K, usize)` — the key and its weight
/// together. This avoids a parallel weights vec and the fragile
/// `IterIndices` helper that had to duplicate linked list internals.
///
/// The `index` hashmap maps `u64_hash → list_index` for O(1) lookup.
struct ShardState<K: Default + Clone> {
    /// LRU ordering. Each node holds `(key, weight)`.
    order: LinkedList<(K, usize)>,
    /// Maps u64 hash of K → list_index for O(1) promote/lookup.
    index: HashMap<u64, usize>,
    /// Total weight in this shard.
    used_weight: usize,
}

impl<K: Hash + Default + Clone> ShardState<K> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            order: LinkedList::with_capacity(capacity),
            index: HashMap::with_capacity(capacity),
            used_weight: 0,
        }
    }

    /// Admit a key. Returns `(is_new, old_weight)`.
    fn admit(&mut self, key: K, weight: usize) -> (bool, usize) {
        let weight = weight.max(1);
        let key_hash = hash_key(&key);

        if let Some(&list_index) = self.index.get(&key_hash) {
            let node = self
                .order
                .peek_mut(list_index)
                .expect("index/list out of sync");
            let old_weight = node.1;
            if weight != old_weight {
                self.used_weight += weight;
                self.used_weight -= old_weight;
                node.1 = weight;
            }
            self.order.promote(list_index);
            (false, old_weight)
        } else {
            let list_index = self.order.push_head((key, weight));
            self.index.insert(key_hash, list_index);
            self.used_weight += weight;
            (true, 0)
        }
    }

    /// Add `delta` to a key's weight (capped at `max_weight`) and promote it,
    /// admitting the key if needed. Returns `(old_weight, new_weight,
    /// admitted)`. Because this runs inside the shard actor — the sole writer
    /// of this shard's weights — the read-add-write is atomic, so concurrent
    /// increments of the same key cannot lose updates.
    fn increment_weight(
        &mut self,
        key: K,
        delta: usize,
        max_weight: Option<usize>,
    ) -> (usize, usize, bool) {
        let key_hash = hash_key(&key);
        if let Some(&list_index) = self.index.get(&key_hash) {
            let node = self
                .order
                .peek_mut(list_index)
                .expect("index/list out of sync");
            let old_weight = node.1;
            let incremented = old_weight.saturating_add(delta);
            let new_weight = max_weight.map_or(incremented, |m| incremented.min(m).max(old_weight));
            if new_weight != old_weight {
                self.used_weight += new_weight;
                self.used_weight -= old_weight;
                node.1 = new_weight;
            }
            self.order.promote(list_index);
            return (old_weight, new_weight, false);
        }

        let weight = max_weight.map_or(delta, |m| delta.min(m)).max(1);
        let list_index = self.order.push_head((key, weight));
        self.index.insert(key_hash, list_index);
        self.used_weight += weight;
        (0, weight, true)
    }

    fn promote(&mut self, key_hash: u64) -> bool {
        if let Some(&list_index) = self.index.get(&key_hash) {
            self.order.promote(list_index);
            true
        } else {
            false
        }
    }

    fn evict(&mut self) -> Option<(K, usize)> {
        let (key, weight) = self.order.pop_tail()?;
        let key_hash = hash_key(&key);
        self.index.remove(&key_hash);
        self.used_weight -= weight;
        Some((key, weight))
    }

    fn remove(&mut self, key_hash: u64) -> Option<(K, usize)> {
        let list_index = self.index.remove(&key_hash)?;
        let (key, weight) = self.order.remove(list_index);
        self.used_weight -= weight;
        Some((key, weight))
    }

    fn insert_tail(&mut self, key: K, weight: usize) -> bool {
        let key_hash = hash_key(&key);
        if self.index.contains_key(&key_hash) {
            return false;
        }
        let list_index = self.order.push_tail((key, weight));
        self.index.insert(key_hash, list_index);
        self.used_weight += weight;
        true
    }

    fn peek_lru(&self) -> Option<(&K, usize)> {
        let idx = self.order.tail()?;
        let (key, weight) = self.order.peek(idx)?;
        Some((key, *weight))
    }

    /// Clone all entries in LRU order (most recent first).
    fn snapshot(&self) -> Vec<(K, usize)> {
        self.order.iter().map(|(k, w)| (k.clone(), *w)).collect()
    }
}

struct ShardHandle<K: Send + 'static> {
    tx: mpsc::UnboundedSender<LruMsg<K>>,
}

/// Shared bookkeeping atomics updated by each shard actor.
///
/// These are `Arc`-shared between the [`AsyncLru`] and all actor tasks
/// so that `weight()`, `len()`, and `shard_len()` are lock-free reads.
pub(crate) struct SharedCounters<const N: usize> {
    weight: AtomicUsize,
    len: AtomicUsize,
    shard_lens: [AtomicUsize; N],
    evicted_weight: AtomicUsize,
    evicted_len: AtomicUsize,
}

impl<const N: usize> SharedCounters<N> {
    fn new() -> Self {
        let mut shard_lens = arrayvec::ArrayVec::<_, N>::new();
        for _ in 0..N {
            shard_lens.push(AtomicUsize::new(0));
        }
        Self {
            weight: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
            shard_lens: shard_lens
                .into_inner()
                .expect("shard_lens ArrayVec filled with exactly N elements"),
            evicted_weight: AtomicUsize::new(0),
            evicted_len: AtomicUsize::new(0),
        }
    }

    fn over_limit(&self, weight_limit: usize, len_watermark: Option<usize>) -> bool {
        self.weight.load(Ordering::Relaxed) > weight_limit
            || len_watermark.is_some_and(|w| self.len.load(Ordering::Relaxed) > w)
    }
}

/// Callback invoked for each evicted `(key, weight)` pair.
///
/// Typically used to purge the evicted asset from storage. The callback
/// is dispatched to a bounded pool of workers via an unbounded MPMC
/// channel; each worker awaits the future returned by [`call`](Self::call),
/// so the callback itself decides whether to do its work inline, on a
/// spawned task, or via `spawn_blocking`.
pub trait AsyncEvictionCallback<K>: Send + Sync + 'static {
    /// Invoked for each evicted `(key, weight)` pair. The returned future
    /// is awaited by the callback worker.
    fn call(&self, key: K, weight: usize) -> impl Future<Output = ()> + Send;
}

/// Blanket impl: any `Fn(K, usize) -> impl Future<Output = ()>` is an
/// [`AsyncEvictionCallback`].
impl<K, F, Fut> AsyncEvictionCallback<K> for F
where
    F: Fn(K, usize) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    fn call(&self, key: K, weight: usize) -> impl Future<Output = ()> + Send {
        (self)(key, weight)
    }
}

/// An async LRU with `N` shards, actor-based ordering, and lock-free reads.
///
/// Construct via the builder pattern: call [`AsyncLru::builder`] with the
/// required arguments (weight limit, eviction callback, and shutdown watch),
/// then chain optional setters before calling
/// [`AsyncLruBuilder::build`].
///
/// An [`AsyncEvictionCallback`] is **required** — it is invoked for every
/// item evicted from the LRU. Eviction is driven asynchronously by
/// dedicated eviction worker tasks using power-of-two-choices (P2C) shard
/// selection.
pub struct AsyncLru<K: Send + Hash + Eq + 'static, const N: usize> {
    map: Arc<flurry::HashMap<u64, usize>>,
    shards: [ShardHandle<K>; N],
    counters: Arc<SharedCounters<N>>,
    weight_limit: usize,
    len_watermark: Option<usize>,
    /// Send side of the eviction watch. Updating this wakes the eviction
    /// workers which then drive P2C eviction across shards.
    eviction_trigger: watch::Sender<Instant>,
}

/// Builder for [`AsyncLru`].
///
/// # Required
/// - `weight_limit` — maximum total weight before eviction
/// - `eviction_cb` — callback invoked for each evicted `(key, weight)` pair
/// - `shutdown` — watch receiver that signals actors to stop
///
/// # Example
/// ```ignore
/// let lru = AsyncLru::<String, 32>::builder(1_000_000, eviction_cb, shutdown_rx, runtime_handle)
///     .capacity(10_000)
///     .num_eviction_workers(4)
///     .build();
/// ```
pub struct AsyncLruBuilder<K: Send + Hash + Eq + 'static, C, const N: usize> {
    weight_limit: usize,
    eviction_cb: Arc<C>,
    _key: PhantomData<fn() -> K>,
    shutdown: watch::Receiver<bool>,
    capacity: usize,
    len_watermark: Option<usize>,
    num_eviction_workers: usize,
    num_callback_workers: usize,
    /// Runtime handle for spawning all internal tasks (shard actors,
    /// eviction workers, callback workers).
    runtime: tokio::runtime::Handle,
}

impl<K, C, const N: usize> AsyncLruBuilder<K, C, N>
where
    K: Send + Sync + Hash + Eq + Default + Clone + 'static,
    C: AsyncEvictionCallback<K>,
{
    /// Set the estimated per-shard capacity for preallocation.
    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Set an optional item-count watermark. Eviction will also trigger
    /// when the total item count exceeds this value.
    pub fn len_watermark(mut self, watermark: usize) -> Self {
        self.len_watermark = Some(watermark);
        self
    }

    /// Set the number of concurrent eviction worker tasks.
    ///
    /// Each worker independently watches the eviction trigger and drives
    /// P2C eviction. Defaults to 1. Use higher values for higher eviction
    /// throughput under sustained overload.
    pub fn num_eviction_workers(mut self, n: usize) -> Self {
        self.num_eviction_workers = n;
        self
    }

    /// Set the number of concurrent callback worker tasks that execute
    /// the [`AsyncEvictionCallback`].
    ///
    /// These workers pull evicted `(key, weight)` pairs from an unbounded
    /// channel and drive the async callback. This bounds the number of
    /// in-flight eviction callbacks without blocking the shard actors.
    ///
    /// Defaults to 1.
    pub fn num_callback_workers(mut self, n: usize) -> Self {
        self.num_callback_workers = n;
        self
    }

    fn build_inner(self) -> (AsyncLru<K, N>, async_channel::Receiver<(K, usize)>) {
        let map = Arc::new(flurry::HashMap::new());
        let counters = Arc::new(SharedCounters::new());
        let (eviction_trigger, eviction_rx) = watch::channel(Instant::now());

        // Unbounded MPMC channel for dispatching eviction callbacks.
        // Shard actors send (key, weight) here; callback workers consume.
        // async_channel is multi-consumer so no mutex is needed.
        let (cb_tx, cb_rx) = async_channel::unbounded::<(K, usize)>();

        let handle = self.runtime;

        // One actor per shard. Each shard gets its own channel and
        // dedicated actor task.
        let mut shard_handles = arrayvec::ArrayVec::<_, N>::new();

        for shard_idx in 0..N {
            let (tx, rx) = mpsc::unbounded_channel();
            let ctx = ShardContext {
                map: Arc::clone(&map),
                shard_idx,
                counters: Arc::clone(&counters),
                weight_limit: self.weight_limit,
                len_watermark: self.len_watermark,
                eviction_dispatch: cb_tx.clone(),
                eviction_trigger: eviction_trigger.clone(),
            };
            shard_handles.push(ShardHandle { tx: tx.clone() });
            handle.spawn(shard_actor::<K, N>(
                rx,
                ctx,
                self.capacity,
                self.shutdown.clone(),
            ));
        }
        let handles = shard_handles;

        // Spawn eviction workers — each watches the trigger and drives
        // P2C eviction independently.
        {
            let shard_txs: Vec<_> = handles.iter().map(|h| h.tx.clone()).collect();
            for _ in 0..self.num_eviction_workers {
                let counters = Arc::clone(&counters);
                handle.spawn(eviction_worker::<K, N>(
                    eviction_rx.clone(),
                    shard_txs.clone(),
                    counters,
                    self.weight_limit,
                    self.len_watermark,
                    self.shutdown.clone(),
                ));
            }
        }

        // Spawn callback workers — each pulls (key, weight) from the
        // shared MPMC channel and drives the async eviction callback.
        // This bounds in-flight callback concurrency to num_callback_workers.
        for _ in 0..self.num_callback_workers {
            let rx = cb_rx.clone();
            let cb = Arc::clone(&self.eviction_cb);
            handle.spawn(callback_worker(rx, cb));
        }

        let lru = AsyncLru {
            map,
            shards: handles
                .into_inner()
                .map_err(|_| ())
                .expect("shard handles ArrayVec filled with exactly N elements"),
            counters,
            weight_limit: self.weight_limit,
            len_watermark: self.len_watermark,
            eviction_trigger,
        };

        (lru, cb_rx)
    }
}

impl<K, C, const N: usize> AsyncLruBuilder<K, C, N>
where
    K: Send + Sync + Hash + Eq + Default + Clone + 'static,
    C: AsyncEvictionCallback<K>,
{
    /// Build and return the eviction callback channel receiver.
    ///
    /// This is the same unbounded MPMC channel that callback workers
    /// read from. Callers can use this to drive eviction callbacks on
    /// dedicated OS threads (via [`async_channel::Receiver::recv_blocking`])
    /// instead of — or in addition to — the built-in callback workers.
    ///
    /// Set [`num_callback_workers`](Self::num_callback_workers) to 0
    /// if you want to handle all callbacks externally.
    pub fn build_with_eviction_receiver(
        self,
    ) -> (AsyncLru<K, N>, async_channel::Receiver<(K, usize)>) {
        self.build_inner()
    }

    /// Standard build — discards the eviction receiver.
    pub fn build(self) -> AsyncLru<K, N> {
        self.build_inner().0
    }
}

impl<K, const N: usize> AsyncLru<K, N>
where
    K: Send + Sync + Hash + Eq + Default + Clone + 'static,
{
    /// Create a builder for constructing an [`AsyncLru`].
    ///
    /// This is the **only** way to construct an `AsyncLru`. All four
    /// arguments are required:
    ///
    /// - `weight_limit` — maximum total weight before eviction is triggered.
    /// - `eviction_cb` — callback invoked for every evicted `(key, weight)`.
    /// - `shutdown` — a [`tokio::sync::watch::Receiver<bool>`] that signals
    ///   all shard actors and eviction workers to stop.
    /// - `runtime` — a [`tokio::runtime::Handle`] on which all internal
    ///   tasks (shard actors, eviction workers, callback workers) are
    ///   spawned. The public API methods ([`admit`](Self::admit),
    ///   [`promote`](Self::promote), [`peek`](Self::peek), etc.) still
    ///   execute on the caller's thread.
    pub fn builder<C>(
        weight_limit: usize,
        eviction_cb: Arc<C>,
        shutdown: watch::Receiver<bool>,
        runtime: tokio::runtime::Handle,
    ) -> AsyncLruBuilder<K, C, N>
    where
        C: AsyncEvictionCallback<K>,
    {
        AsyncLruBuilder {
            weight_limit,
            eviction_cb,
            _key: PhantomData,
            shutdown,
            capacity: 0,
            len_watermark: None,
            num_eviction_workers: 1,
            num_callback_workers: 1,
            runtime,
        }
    }

    // ---- Fire-and-forget (hot path) ----

    /// Admit a key with the given weight, or update the weight if the key
    /// already exists. Fire-and-forget.
    ///
    /// The message is enqueued on the shard's unbounded channel, so this
    /// never blocks and is never dropped (it fails only if the actor is
    /// gone, i.e. during shutdown). There is no backpressure: a sustained
    /// admit rate above the shard actor's drain rate grows the queue.
    ///
    /// If the new total weight exceeds the configured limit, eviction is
    /// triggered asynchronously by the eviction workers.
    ///
    /// Returns the shard index that owns this key.
    pub fn admit(&self, key: K, weight: usize) -> usize {
        let shard = get_shard(&key, N);
        send_msg(
            &self.shards[shard].tx,
            LruMsg::Admit {
                key,
                weight: weight.max(1),
            },
        );
        shard
    }

    /// Promote a key to the head of its shard's LRU.
    ///
    /// Fire-and-forget. The message is enqueued on the shard's unbounded
    /// channel: this never blocks the hot read path and is never dropped
    /// (it fails only if the actor is gone, i.e. during shutdown).
    pub fn promote(&self, key: &K) -> bool {
        let key_hash = hash_key(key);
        let guard = self.map.guard();
        let exists = self.map.contains_key(&key_hash, &guard);
        drop(guard);

        if exists {
            let shard = get_shard_from_hash(key_hash, N);
            send_msg(&self.shards[shard].tx, LruMsg::Promote { key_hash });
        }
        exists
    }

    /// Insert a key at the **tail** of its shard's LRU list (for
    /// deserialization / bulk loading).
    ///
    /// Like [`admit`](Self::admit), the message is enqueued on the shard's
    /// unbounded channel and is never dropped.
    ///
    /// Returns the shard index that owns this key.
    pub fn insert_tail(&self, key: K, weight: usize) -> usize {
        let shard = get_shard(&key, N);
        send_msg(&self.shards[shard].tx, LruMsg::InsertTail { key, weight });
        shard
    }

    /// Remove a key from the LRU. Fire-and-forget.
    ///
    /// The message is enqueued on the shard's unbounded channel, so it is
    /// never dropped (it fails only if the actor is gone, i.e. during
    /// shutdown).
    ///
    /// The key is matched by hash. Use [`Self::remove_by_hash`] when the caller already has the
    /// hash, including from an alternate key representation.
    pub fn remove(&self, key: &K) {
        self.remove_by_hash(hash_key(key));
    }

    /// Remove an entry using a precomputed [`hash_key`] result. Fire-and-forget.
    ///
    /// This supports alternate key representations that cannot implement `Borrow<K>`. The hash
    /// must match the `K` originally inserted; it selects both the shard and the entry within it.
    pub fn remove_by_hash(&self, key_hash: u64) {
        let shard = get_shard_from_hash(key_hash, N);
        send_msg(&self.shards[shard].tx, LruMsg::Remove { key_hash });
    }

    /// Increment a key's weight by `delta`, capped at `max_weight`, admitting
    /// it if needed. Fire-and-forget.
    ///
    /// The add is performed inside the owning shard actor against the key's
    /// current weight, so concurrent increments of the same key are serialized
    /// and cannot lose updates. (Computing the new weight on the caller side
    /// from a separate read would race: two callers could read the same value
    /// and both write the same total, dropping one delta.)
    ///
    /// The message is enqueued on the shard's unbounded channel and is never
    /// dropped.
    pub fn increment_weight(&self, key: &K, delta: usize, max_weight: Option<usize>) {
        let shard = get_shard(key, N);
        send_msg(
            &self.shards[shard].tx,
            LruMsg::IncrementWeight {
                key: key.clone(),
                delta,
                max_weight,
            },
        );
    }

    /// Manually trigger eviction.
    ///
    /// Sends a signal via the [`tokio::sync::watch`] channel that wakes all
    /// eviction workers. Each worker then drives P2C eviction across shards
    /// until the total weight is back under the configured limit.
    ///
    /// Normally you do not need to call this — eviction is triggered
    /// automatically by [`admit`](Self::admit). This method is useful after
    /// bulk loading via [`insert_tail`](Self::insert_tail).
    pub fn trigger_eviction(&self) {
        let _ = self.eviction_trigger.send(Instant::now());
    }

    // ---- Lock-free reads ----

    /// Check whether `key` exists in the LRU. Lock-free.
    ///
    /// Reads from the [`flurry::HashMap`] which is updated atomically by the
    /// shard actors. Does **not** promote the key (use [`promote`](Self::promote)
    /// for that).
    pub fn peek(&self, key: &K) -> bool {
        let guard = self.map.guard();
        self.map.contains_key(&hash_key(key), &guard)
    }

    /// Return the weight of `key`, or `None` if the key is absent. Lock-free.
    ///
    /// Reads from the [`flurry::HashMap`]. Does **not** promote the key.
    pub fn peek_weight(&self, key: &K) -> Option<usize> {
        let guard = self.map.guard();
        self.map.get(&hash_key(key), &guard).copied()
    }

    /// Peek at the least-recently-used key in the given shard.
    ///
    /// Sends a request/response message to the shard actor and awaits
    /// the reply. Returns `None` if `shard >= N` (out of bounds) or if
    /// the shard is empty.
    pub async fn peek_lru(&self, shard: usize) -> Option<(K, usize)> {
        if shard >= N {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        send_msg(&self.shards[shard].tx, LruMsg::PeekLru { resp: tx });
        rx.await.ok().flatten()
    }

    // ---- Shard queries ----

    /// Snapshot the contents of a shard in LRU order (MRU first).
    ///
    /// Sends a request to the shard actor, which clones all keys and
    /// returns them with their weights. Returns `None` if `shard >= N`
    /// (out of bounds).
    ///
    /// Because every key is cloned, this is an expensive operation and
    /// should only be used for serialization or diagnostics.
    pub async fn snapshot_shard(&self, shard: usize) -> Option<Vec<(K, usize)>> {
        if shard >= N {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        send_msg(&self.shards[shard].tx, LruMsg::Snapshot { resp: tx });
        rx.await.ok()
    }

    /// Lock-free, best-effort read of the item count for a single shard.
    ///
    /// Uses a `Relaxed` atomic load, so the value may be slightly stale
    /// under concurrent mutations. Suitable for P2C heuristics where
    /// approximate values are acceptable.
    pub fn shard_len(&self, shard: usize) -> usize {
        self.counters.shard_lens[shard].load(Ordering::Relaxed)
    }

    /// Query the exact weight of a shard via request/response to the actor.
    ///
    /// Unlike the global [`weight`](Self::weight) counter (which is lock-free
    /// but approximate), this returns the shard's authoritative `used_weight`
    /// as maintained by its actor. Returns `None` if `shard >= N` (out of
    /// bounds).
    pub async fn shard_weight(&self, shard: usize) -> Option<usize> {
        if shard >= N {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        send_msg(&self.shards[shard].tx, LruMsg::QueryWeight { resp: tx });
        rx.await.ok()
    }

    /// Total number of shards.
    pub const fn shards(&self) -> usize {
        N
    }

    // ---- Global counters (lock-free) ----

    /// Current total weight across all shards. Lock-free `Relaxed` load.
    ///
    /// May be slightly stale under concurrent mutations.
    pub fn weight(&self) -> usize {
        self.counters.weight.load(Ordering::Relaxed)
    }

    /// Current total item count across all shards. Lock-free `Relaxed` load.
    ///
    /// May be slightly stale under concurrent mutations.
    pub fn len(&self) -> usize {
        self.counters.len.load(Ordering::Relaxed)
    }

    /// Whether the LRU contains no items. Lock-free.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Accumulated weight of all items evicted since construction. Lock-free.
    pub fn evicted_weight(&self) -> usize {
        self.counters.evicted_weight.load(Ordering::Relaxed)
    }

    /// Accumulated count of all items evicted since construction. Lock-free.
    pub fn evicted_len(&self) -> usize {
        self.counters.evicted_len.load(Ordering::Relaxed)
    }

    /// The configured maximum weight limit. Eviction is triggered when the
    /// total weight exceeds this value.
    pub fn weight_limit(&self) -> usize {
        self.weight_limit
    }

    /// Whether the LRU is currently over its weight limit or item-count
    /// watermark. Lock-free.
    pub fn over_limit(&self) -> bool {
        self.counters
            .over_limit(self.weight_limit, self.len_watermark)
    }

    /// Save all shards sequentially, one at a time from start to finish.
    ///
    /// For each shard: snapshot via the actor, then call `write_shard`
    /// on a blocking thread. This keeps memory usage bounded (only one
    /// shard's snapshot in memory at a time) and avoids contention.
    /// Returns the number of shards successfully written.
    pub async fn save<F>(&self, write_shard: F) -> usize
    where
        F: Fn(ShardData<K>) -> Result<(), String> + Send + Sync + 'static,
    {
        let write_shard = Arc::new(write_shard);
        let mut success_count = 0usize;

        for i in 0..N {
            let items = match self.snapshot_shard(i).await {
                Some(items) => items,
                None => continue,
            };
            let shard_data = ShardData {
                shard_index: i,
                items,
            };
            let wf = Arc::clone(&write_shard);
            match tokio::task::spawn_blocking(move || wf(shard_data)).await {
                Ok(Ok(())) => success_count += 1,
                Ok(Err(e)) => log::error!("AsyncLru: failed to write shard {i}: {e}"),
                Err(e) => log::error!("AsyncLru: write shard {i} panicked: {e}"),
            }
        }

        success_count
    }
}

// ---------------------------------------------------------------------------
// Shard actor
// ---------------------------------------------------------------------------

/// Immutable context shared by the actor loop and message handler.
struct ShardContext<K: Send + 'static, const N: usize> {
    map: Arc<flurry::HashMap<u64, usize>>,
    shard_idx: usize,
    counters: Arc<SharedCounters<N>>,
    weight_limit: usize,
    len_watermark: Option<usize>,
    /// Unbounded sender for dispatching eviction work to the callback
    /// worker pool. Never blocks the shard actor.
    eviction_dispatch: async_channel::Sender<(K, usize)>,
    eviction_trigger: watch::Sender<Instant>,
}

/// Maximum number of messages the shard actor drains per `recv_many`
/// call before yielding back to the tokio scheduler. This amortises the
/// cost of waking the task across a batch of messages while preventing the
/// actor from monopolizing a worker thread.
const DRAIN_BATCH: usize = 64;

/// The actor loop for a single shard.
///
/// Uses [`mpsc::UnboundedReceiver::recv_many`] to drain up to
/// [`DRAIN_BATCH`] messages per wakeup, which keeps the unbounded queue
/// short under load by raising drain throughput.
async fn shard_actor<K, const N: usize>(
    mut rx: mpsc::UnboundedReceiver<LruMsg<K>>,
    ctx: ShardContext<K, N>,
    capacity: usize,
    mut shutdown: watch::Receiver<bool>,
) where
    K: Send + Sync + Hash + Eq + Default + Clone + 'static,
{
    let mut state = ShardState::<K>::with_capacity(capacity);
    let mut batch: Vec<LruMsg<K>> = Vec::with_capacity(DRAIN_BATCH);

    loop {
        // Block until at least one message arrives (or shutdown), then
        // drain up to DRAIN_BATCH messages in one batch.
        let n = tokio::select! {
            n = rx.recv_many(&mut batch, DRAIN_BATCH) => n,
            _ = shutdown.changed() => break,
        };
        if n == 0 {
            // All senders dropped and the queue is drained.
            break;
        }
        for msg in batch.drain(..) {
            process_msg(&mut state, &ctx, msg);
        }
    }
}

/// Signal the eviction actor if the global weight/count is over the limit.
fn maybe_signal_eviction<K: Send + 'static, const N: usize>(ctx: &ShardContext<K, N>) {
    if ctx.counters.over_limit(ctx.weight_limit, ctx.len_watermark) {
        let _ = ctx.eviction_trigger.send(Instant::now());
    }
}

/// Shared admit logic used by both `Admit` and `AdmitSync`.
fn admit_inner<K, const N: usize>(
    state: &mut ShardState<K>,
    ctx: &ShardContext<K, N>,
    key: K,
    weight: usize,
) where
    K: Send + Sync + Hash + Eq + Default + Clone + 'static,
{
    // hash_key is computed here and again inside ShardState::admit. This is
    // intentional: ShardState is a self-contained data structure that doesn't
    // know about the flurry map, so it computes its own hash. The cost is one
    // extra ahash per admit — negligible compared to the channel send.
    let key_hash = hash_key(&key);
    let (is_new, old_weight) = state.admit(key, weight);
    let weight = weight.max(1);

    let guard = ctx.map.guard();
    ctx.map.insert(key_hash, weight, &guard);
    drop(guard);

    ctx.counters.weight.fetch_add(weight, Ordering::Relaxed);
    if old_weight > 0 {
        ctx.counters.weight.fetch_sub(old_weight, Ordering::Relaxed);
    }
    if is_new {
        ctx.counters.len.fetch_add(1, Ordering::Relaxed);
        ctx.counters.shard_lens[ctx.shard_idx].fetch_add(1, Ordering::Relaxed);
    }
}

/// Process a single actor message.
fn process_msg<K, const N: usize>(
    state: &mut ShardState<K>,
    ctx: &ShardContext<K, N>,
    msg: LruMsg<K>,
) where
    K: Send + Sync + Hash + Eq + Default + Clone + 'static,
{
    match msg {
        LruMsg::Admit { key, weight } => {
            admit_inner(state, ctx, key, weight);
            maybe_signal_eviction(ctx);
        }
        LruMsg::IncrementWeight {
            key,
            delta,
            max_weight,
        } => {
            let key_hash = hash_key(&key);
            let (old_weight, new_weight, admitted) = state.increment_weight(key, delta, max_weight);
            if new_weight != old_weight {
                let guard = ctx.map.guard();
                ctx.map.insert(key_hash, new_weight, &guard);
                drop(guard);
                // Reconcile the shared weight counter by the net change.
                ctx.counters.weight.fetch_add(new_weight, Ordering::Relaxed);
                ctx.counters.weight.fetch_sub(old_weight, Ordering::Relaxed);
            }
            if admitted {
                ctx.counters.len.fetch_add(1, Ordering::Relaxed);
                ctx.counters.shard_lens[ctx.shard_idx].fetch_add(1, Ordering::Relaxed);
            }
            maybe_signal_eviction(ctx);
        }
        LruMsg::Promote { key_hash } => {
            state.promote(key_hash);
        }
        LruMsg::Remove { key_hash } => {
            if let Some((_, weight)) = state.remove(key_hash) {
                let guard = ctx.map.guard();
                ctx.map.remove(&key_hash, &guard);
                ctx.counters.weight.fetch_sub(weight, Ordering::Relaxed);
                ctx.counters.len.fetch_sub(1, Ordering::Relaxed);
                ctx.counters.shard_lens[ctx.shard_idx].fetch_sub(1, Ordering::Relaxed);
            }
        }
        LruMsg::InsertTail { key, weight } => {
            let key_hash = hash_key(&key);
            if state.insert_tail(key, weight) {
                let guard = ctx.map.guard();
                ctx.map.insert(key_hash, weight, &guard);
                ctx.counters.weight.fetch_add(weight, Ordering::Relaxed);
                ctx.counters.len.fetch_add(1, Ordering::Relaxed);
                ctx.counters.shard_lens[ctx.shard_idx].fetch_add(1, Ordering::Relaxed);
            }
        }
        LruMsg::Evict => {
            if let Some((key, weight)) = state.evict() {
                let key_hash = hash_key(&key);
                let guard = ctx.map.guard();
                ctx.map.remove(&key_hash, &guard);
                drop(guard);
                ctx.counters.weight.fetch_sub(weight, Ordering::Relaxed);
                ctx.counters.len.fetch_sub(1, Ordering::Relaxed);
                ctx.counters.shard_lens[ctx.shard_idx].fetch_sub(1, Ordering::Relaxed);
                ctx.counters
                    .evicted_weight
                    .fetch_add(weight, Ordering::Relaxed);
                ctx.counters.evicted_len.fetch_add(1, Ordering::Relaxed);
                // Send to the callback worker pool. Unbounded so the
                // shard actor is never blocked. The workers drive the
                // async callback with bounded concurrency.
                let _ = ctx.eviction_dispatch.try_send((key, weight));
            }
        }
        LruMsg::PeekLru { resp } => {
            let result = state.peek_lru().map(|(k, w)| (k.clone(), w));
            let _ = resp.send(result);
        }
        LruMsg::Snapshot { resp } => {
            let _ = resp.send(state.snapshot());
        }
        LruMsg::QueryWeight { resp } => {
            let _ = resp.send(state.used_weight);
        }
    }
}

// ---------------------------------------------------------------------------
// Eviction actor
// ---------------------------------------------------------------------------

/// A single eviction worker. Multiple workers can run concurrently.
/// Each one watches the eviction trigger, picks a shard via P2C, sends
/// an `Evict` message, and yields before re-checking the limit. The send
/// is non-blocking (unbounded channel), so pacing comes from the
/// `yield_now` between sends rather than from channel backpressure.
async fn eviction_worker<K, const N: usize>(
    mut trigger_rx: watch::Receiver<Instant>,
    shard_txs: Vec<mpsc::UnboundedSender<LruMsg<K>>>,
    counters: Arc<SharedCounters<N>>,
    weight_limit: usize,
    len_watermark: Option<usize>,
    mut shutdown: watch::Receiver<bool>,
) where
    K: Send + Sync + Hash + Eq + Default + Clone + 'static,
{
    let mut rng = rand::rngs::StdRng::from_entropy();

    loop {
        tokio::select! {
            result = trigger_rx.changed() => {
                if result.is_err() { break; }
            }
            _ = shutdown.changed() => break,
        }

        // Evict until under the limit. Each iteration enqueues one Evict
        // message to the more-loaded of two randomly-picked shards (P2C),
        // then yields so the shard actor can process it and update the
        // counters before we re-check the limit.
        while counters.over_limit(weight_limit, len_watermark) {
            let shard = if N <= 1 {
                0
            } else {
                let a = rng.gen_range(0..N);
                let b = rng.gen_range(0..N);
                if counters.shard_lens[a].load(Ordering::Relaxed)
                    >= counters.shard_lens[b].load(Ordering::Relaxed)
                {
                    a
                } else {
                    b
                }
            };

            // Unbounded send: never blocks, never drops.
            let _ = shard_txs[shard].send(LruMsg::Evict);

            // Yield to let the shard process the evict and update counters.
            tokio::task::yield_now().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Callback worker — drives eviction callbacks with bounded concurrency
// ---------------------------------------------------------------------------

/// A worker that pulls evicted `(key, weight)` pairs from the shared
/// MPMC channel and awaits the [`AsyncEvictionCallback`]'s returned
/// future for each.
///
/// The callback decides how its work runs (inline, on a spawned task, or
/// offloaded via `spawn_blocking`); the worker simply awaits the future
/// it returns.
///
/// Multiple workers share the same `async_channel::Receiver` (it is
/// multi-consumer), so the total in-flight callback concurrency equals
/// the number of workers. No mutex needed.
async fn callback_worker<K, C>(rx: async_channel::Receiver<(K, usize)>, cb: Arc<C>)
where
    K: Send + 'static,
    C: AsyncEvictionCallback<K>,
{
    while let Ok((key, weight)) = rx.recv().await {
        cb.call(key, weight).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn wait() {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    fn key(n: u32) -> String {
        format!("key-{n}")
    }

    /// Helper: create an AsyncLru with a no-op eviction callback.
    /// Returns the AsyncLru and the shutdown sender (keep it alive for the
    /// duration of the test).
    fn make_lru<const S: usize>(
        weight_limit: usize,
        capacity: usize,
    ) -> (AsyncLru<String, S>, watch::Sender<bool>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let lru = AsyncLru::builder(
            weight_limit,
            Arc::new(|_key: String, _weight| async {}),
            shutdown_rx,
            tokio::runtime::Handle::current(),
        )
        .capacity(capacity)
        .build();
        (lru, shutdown_tx)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn basic_admit_and_peek() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);

        lru.admit(key(1), 10);
        lru.admit(key(2), 20);
        lru.admit(key(3), 30);
        wait();

        assert!(lru.peek(&key(1)));
        assert!(lru.peek(&key(2)));
        assert!(lru.peek(&key(3)));
        assert!(!lru.peek(&key(999)));

        assert_eq!(lru.peek_weight(&key(1)), Some(10));
        assert_eq!(lru.peek_weight(&key(2)), Some(20));
        assert_eq!(lru.peek_weight(&key(3)), Some(30));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admit_updates_weight() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);

        lru.admit(key(1), 10);
        wait();
        assert_eq!(lru.peek_weight(&key(1)), Some(10));

        lru.admit(key(1), 25);
        wait();
        assert_eq!(lru.peek_weight(&key(1)), Some(25));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_works() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);

        lru.admit(key(1), 10);
        lru.admit(key(2), 20);
        wait();

        lru.remove(&key(1));
        lru.remove_by_hash(hash_key(&key(2)));
        wait();
        assert!(!lru.peek(&key(1)));
        assert!(!lru.peek(&key(2)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn promote_is_best_effort() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);
        lru.admit(key(1), 10);
        wait();

        assert!(lru.promote(&key(1)));
        assert!(!lru.promote(&key(999)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn zero_weight_becomes_one() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);
        lru.admit(key(1), 0);
        wait();
        assert_eq!(lru.peek_weight(&key(1)), Some(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn insert_tail_for_deserialization() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);

        lru.insert_tail(key(1), 100);
        lru.insert_tail(key(2), 200);
        wait();

        assert_eq!(lru.peek_weight(&key(1)), Some(100));
        assert_eq!(lru.peek_weight(&key(2)), Some(200));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn increment_weight_works() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);
        lru.admit(key(1), 10);
        wait();

        lru.increment_weight(&key(1), 5, None);
        wait();
        assert_eq!(lru.peek_weight(&key(1)), Some(15));

        lru.increment_weight(&key(1), 100, Some(20));
        wait();
        assert_eq!(lru.peek_weight(&key(1)), Some(20));

        // A cap below the current weight must not shrink the item.
        lru.increment_weight(&key(1), 1, Some(10));
        wait();
        assert_eq!(lru.peek_weight(&key(1)), Some(20));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn increment_weight_missing_key_is_admitted() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);
        lru.increment_weight(&key(99), 5, None);
        let shard = get_shard(&key(99), 4);
        lru.shard_weight(shard).await;
        assert_eq!(lru.peek_weight(&key(99)), Some(5));
        assert_eq!(lru.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peek_lru_returns_tail() {
        let (lru, _shutdown) = make_lru::<1>(1000, 64);

        lru.admit(key(1), 1);
        lru.admit(key(2), 1);
        lru.admit(key(3), 1);
        wait();

        let tail = lru.peek_lru(0).await;
        assert_eq!(tail, Some((key(1), 1)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn snapshot_shard_preserves_order() {
        let (lru, _shutdown) = make_lru::<1>(1000, 64);

        lru.admit(key(1), 5);
        lru.admit(key(2), 10);
        lru.admit(key(3), 15);
        wait();

        let items = lru.snapshot_shard(0).await.unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], (key(3), 15));
        assert_eq!(items[1], (key(2), 10));
        assert_eq!(items[2], (key(1), 5));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shard_len_and_weight() {
        let (lru, _shutdown) = make_lru::<1>(1000, 64);

        lru.admit(key(1), 10);
        lru.admit(key(2), 20);
        wait();

        assert_eq!(lru.shard_len(0), 2);
        assert_eq!(lru.shard_weight(0).await, Some(30));
        assert_eq!(lru.shards(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn global_counters_are_maintained() {
        let (lru, _shutdown) = make_lru::<2>(1000, 64);

        lru.admit(key(1), 10);
        lru.admit(key(2), 20);
        lru.admit(key(3), 30);
        wait();

        assert_eq!(lru.weight(), 60);
        assert_eq!(lru.len(), 3);

        // Re-admit with different weight
        lru.admit(key(1), 5);
        wait();
        assert_eq!(lru.weight(), 55);
        assert_eq!(lru.len(), 3);

        // Remove (fire-and-forget, wait for actor)
        lru.remove(&key(2));
        wait();
        assert_eq!(lru.weight(), 35);
        assert_eq!(lru.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eviction_is_triggered_when_over_limit() {
        use std::sync::atomic::AtomicUsize;

        let evicted_count = Arc::new(AtomicUsize::new(0));
        let evicted_clone = evicted_count.clone();

        let cb = move |_key: String, _weight| {
            let evicted_clone = Arc::clone(&evicted_clone);
            async move {
                evicted_clone.fetch_add(1, Ordering::SeqCst);
            }
        };

        // Weight limit of 20, 1 shard for determinism.
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let lru = AsyncLru::<String, 1>::builder(
            20,
            Arc::new(cb),
            shutdown_rx,
            tokio::runtime::Handle::current(),
        )
        .capacity(64)
        .build();

        // Admit 30 weight — should trigger eviction of at least 10 weight.
        lru.admit(key(1), 10);
        lru.admit(key(2), 10);
        lru.admit(key(3), 10);
        wait();

        assert!(
            evicted_count.load(Ordering::SeqCst) >= 1,
            "at least one item should have been evicted"
        );
        assert!(
            lru.weight() <= 20,
            "weight {} should be at or below limit 20",
            lru.weight()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eviction_callback_receives_evicted_items() {
        let evicted_items = Arc::new(std::sync::Mutex::new(vec![]));
        let evicted_clone = evicted_items.clone();

        let cb = move |key: String, weight| {
            let evicted_clone = Arc::clone(&evicted_clone);
            async move {
                evicted_clone.lock().unwrap().push((key, weight));
            }
        };

        // Limit 15, admit 30 → must evict ~15 weight.
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let lru = AsyncLru::<String, 1>::builder(
            15,
            Arc::new(cb),
            shutdown_rx,
            tokio::runtime::Handle::current(),
        )
        .capacity(64)
        .build();

        lru.admit(key(1), 10);
        lru.admit(key(2), 10);
        lru.admit(key(3), 10);
        wait();

        let items = evicted_items.lock().unwrap();
        let evicted_weight: usize = items.iter().map(|(_, w)| *w).sum();
        assert!(
            evicted_weight >= 10,
            "expected at least 10 weight evicted, got {evicted_weight}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_calls_write_shard_for_each_shard() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);

        lru.admit(key(1), 5);
        lru.admit(key(2), 10);
        lru.admit(key(3), 15);
        wait();

        let collected = Arc::new(std::sync::Mutex::new(vec![]));
        let collected_clone = collected.clone();

        let success = lru
            .save(move |shard_data| {
                collected_clone
                    .lock()
                    .unwrap()
                    .push((shard_data.shard_index, shard_data.items));
                Ok(())
            })
            .await;

        assert_eq!(success, 4);
        let data = collected.lock().unwrap();
        let mut shard_indices: Vec<_> = data.iter().map(|(idx, _)| *idx).collect();
        shard_indices.sort();
        assert_eq!(shard_indices, vec![0, 1, 2, 3]);
        let total_items: usize = data.iter().map(|(_, items)| items.len()).sum();
        assert_eq!(total_items, 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_handles_write_errors() {
        let (lru, _shutdown) = make_lru::<4>(1000, 64);
        lru.admit(key(1), 5);
        wait();

        let success = lru
            .save(|shard_data| {
                if shard_data.shard_index == 2 {
                    Err("disk full".to_string())
                } else {
                    Ok(())
                }
            })
            .await;

        assert_eq!(success, 3);
    }
}
