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

use pingora_cache::{
    eviction::{simple_lru::Manager, EvictionManager},
    CacheKey,
};

const ITEMS: usize = 5 * usize::pow(2, 20);

/*
   Total:     704,643,412 bytes (100%, 29,014,058.85/s) in 10,485,787 blocks (100%, 431,757.73/s), avg size 67.2 bytes, avg lifetime 6,163,799.09 µs (25.38% of program duration)
   At t-gmax: 520,093,936 bytes (100%) in 5,242,886 blocks (100%), avg size 99.2 bytes
  ├── PP 1.1/4 {
  │     Total:     377,487,360 bytes (53.57%, 15,543,238.31/s) in 5,242,880 blocks (50%, 215,878.31/s), avg size 72 bytes, avg lifetime 12,327,602.83 µs (50.76% of program duration)
  │     Max:       377,487,360 bytes in 5,242,880 blocks, avg size 72 bytes
  │     At t-gmax: 377,487,360 bytes (72.58%) in 5,242,880 blocks (100%), avg size 72 bytes
  │     At t-end:  0 bytes (0%) in 0 blocks (0%), avg size 0 bytes
  │     Allocated at {
  │       #1: 0x5555791dd7e0: alloc::alloc::exchange_malloc (alloc/src/alloc.rs:326:11)
  │       #2: 0x5555791dd7e0: alloc::boxed::Box<T>::new (alloc/src/boxed.rs:215:9)
  │       #3: 0x5555791dd7e0: lru::LruCache<K,V,S>::replace_or_create_node (lru-0.8.1/src/lib.rs:391:20)
  │       #4: 0x5555791dd7e0: lru::LruCache<K,V,S>::capturing_put (lru-0.8.1/src/lib.rs:355:44)
  │       #5: 0x5555791dd7e0: lru::LruCache<K,V,S>::push (lru-0.8.1/src/lib.rs:334:9)
  │       #6: 0x5555791dd7e0: pingora_cache::eviction::simple_lru::Manager::insert (src/eviction/simple_lru.rs:49:23)
  │       #7: 0x5555791dd7e0: <pingora_cache::eviction::simple_lru::Manager as pingora_cache::eviction::EvictionManager>::admit (src/eviction/simple_lru.rs:166:9)
  │       #8: 0x5555791dd7e0: simple_lru_memory::main (pingora-cache/benches/simple_lru_memory.rs:21:9)
  │     }
  │   }
  ├── PP 1.2/4 {
  │     Total:     285,212,780 bytes (40.48%, 11,743,784.5/s) in 22 blocks (0%, 0.91/s), avg size 12,964,217.27 bytes, avg lifetime 1,116,774.23 µs (4.6% of program duration)
  │     Max:       213,909,520 bytes in 2 blocks, avg size 106,954,760 bytes
  │     At t-gmax: 142,606,344 bytes (27.42%) in 1 blocks (0%), avg size 142,606,344 bytes
  │     At t-end:  0 bytes (0%) in 0 blocks (0%), avg size 0 bytes
  │     Allocated at {
  │       #1: 0x5555791dae20: alloc::alloc::alloc (alloc/src/alloc.rs:95:14)
  │       #2: 0x5555791dae20: <hashbrown::raw::alloc::inner::Global as hashbrown::raw::alloc::inner::Allocator>::allocate (src/raw/alloc.rs:47:35)
  │       #3: 0x5555791dae20: hashbrown::raw::alloc::inner::do_alloc (src/raw/alloc.rs:62:9)
  │       #4: 0x5555791dae20: hashbrown::raw::RawTableInner<A>::new_uninitialized (src/raw/mod.rs:1080:38)
  │       #5: 0x5555791dae20: hashbrown::raw::RawTableInner<A>::fallible_with_capacity (src/raw/mod.rs:1109:30)
  │       #6: 0x5555791dae20: hashbrown::raw::RawTableInner<A>::prepare_resize (src/raw/mod.rs:1353:29)
  │       #7: 0x5555791dae20: hashbrown::raw::RawTableInner<A>::resize_inner (src/raw/mod.rs:1426:29)
  │       #8: 0x5555791dae20: hashbrown::raw::RawTableInner<A>::reserve_rehash_inner (src/raw/mod.rs:1403:13)
  │       #9: 0x5555791dae20: hashbrown::raw::RawTable<T,A>::reserve_rehash (src/raw/mod.rs:680:13)
  │       #10: 0x5555791dde50: hashbrown::raw::RawTable<T,A>::reserve (src/raw/mod.rs:646:16)
  │       #11: 0x5555791dde50: hashbrown::raw::RawTable<T,A>::insert (src/raw/mod.rs:725:17)
  │       #12: 0x5555791dde50: hashbrown::map::HashMap<K,V,S,A>::insert (hashbrown-0.12.3/src/map.rs:1679:13)
  │       #13: 0x5555791dde50: lru::LruCache<K,V,S>::capturing_put (lru-0.8.1/src/lib.rs:361:17)
  │       #14: 0x5555791dde50: lru::LruCache<K,V,S>::push (lru-0.8.1/src/lib.rs:334:9)
  │       #15: 0x5555791dde50: pingora_cache::eviction::simple_lru::Manager::insert (src/eviction/simple_lru.rs:49:23)
  │       #16: 0x5555791dde50: <pingora_cache::eviction::simple_lru::Manager as pingora_cache::eviction::EvictionManager>::admit (src/eviction/simple_lru.rs:166:9)
  │       #17: 0x5555791dde50: simple_lru_memory::main (pingora-cache/benches/simple_lru_memory.rs:21:9)
  │     }
  │   }
*/
fn main() {
    let _profiler = dhat::Profiler::new_heap();
    let manager = Manager::new(ITEMS);
    let unused_ttl = std::time::SystemTime::now();
    for i in 0..ITEMS {
        let item = CacheKey::new("", i.to_string(), "").to_compact();
        manager.admit(item, 1, unused_ttl);
    }
}
