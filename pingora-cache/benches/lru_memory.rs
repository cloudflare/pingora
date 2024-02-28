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
    eviction::{lru::Manager, EvictionManager},
    CacheKey,
};

const ITEMS: usize = 5 * usize::pow(2, 20);

/*
    Total:     681,836,456 bytes (100%, 28,192,797.16/s) in 10,485,845 blocks (100%, 433,572.15/s), avg size 65.02 bytes, avg lifetime 5,935,075.17 µs (24.54% of program duration)
    At t-gmax: 569,114,536 bytes (100%) in 5,242,947 blocks (100%), avg size 108.55 bytes
    At t-end:  88 bytes (100%) in 3 blocks (100%), avg size 29.33 bytes
    Allocated at {
      #0: [root]
    }
  ├── PP 1.1/5 {
  │     Total:     293,601,280 bytes (43.06%, 12,139,921.91/s) in 5,242,880 blocks (50%, 216,784.32/s), avg size 56 bytes, avg lifetime 11,870,032.65 µs (49.08% of program duration)
  │     Max:       293,601,280 bytes in 5,242,880 blocks, avg size 56 bytes
  │     At t-gmax: 293,601,280 bytes (51.59%) in 5,242,880 blocks (100%), avg size 56 bytes
  │     At t-end:  0 bytes (0%) in 0 blocks (0%), avg size 0 bytes
  │     Allocated at {
  │       #1: 0x5555703cf69c: alloc::alloc::exchange_malloc (alloc/src/alloc.rs:326:11)
  │       #2: 0x5555703cf69c: alloc::boxed::Box<T>::new (alloc/src/boxed.rs:215:9)
  │       #3: 0x5555703cf69c: pingora_lru::LruUnit<T>::admit (pingora-lru/src/lib.rs:201:20)
  │       #4: 0x5555703cf69c: pingora_lru::Lru<T,_>::admit (pingora-lru/src/lib.rs:48:26)
  │       #5: 0x5555703cf69c: <pingora_cache::eviction::lru::Manager<_> as pingora_cache::eviction::EvictionManager>::admit (src/eviction/lru.rs:114:9)
  │       #6: 0x5555703cf69c: lru_memory::main (pingora-cache/benches/lru_memory.rs:78:9)
  │     }
  │   }
  ├── PP 1.2/5 {
  │     Total:     203,685,456 bytes (29.87%, 8,422,052.97/s) in 50 blocks (0%, 2.07/s), avg size 4,073,709.12 bytes, avg lifetime 6,842,528.74 µs (28.29% of program duration)
  │     Max:       132,906,576 bytes in 32 blocks, avg size 4,153,330.5 bytes
  │     At t-gmax: 132,906,576 bytes (23.35%) in 32 blocks (0%), avg size 4,153,330.5 bytes
  │     At t-end:  0 bytes (0%) in 0 blocks (0%), avg size 0 bytes
  │     Allocated at {
  │       #1: 0x5555703cec54: <alloc::alloc::Global as core::alloc::Allocator>::allocate (alloc/src/alloc.rs:237:9)
  │       #2: 0x5555703cec54: alloc::raw_vec::RawVec<T,A>::allocate_in (alloc/src/raw_vec.rs:185:45)
  │       #3: 0x5555703cec54: alloc::raw_vec::RawVec<T,A>::with_capacity_in (alloc/src/raw_vec.rs:131:9)
  │       #4: 0x5555703cec54: alloc::vec::Vec<T,A>::with_capacity_in (src/vec/mod.rs:641:20)
  │       #5: 0x5555703cec54: alloc::vec::Vec<T>::with_capacity (src/vec/mod.rs:483:9)
  │       #6: 0x5555703cec54: pingora_lru::linked_list::Nodes::with_capacity (pingora-lru/src/linked_list.rs:50:25)
  │       #7: 0x5555703cec54: pingora_lru::linked_list::LinkedList::with_capacity (pingora-lru/src/linked_list.rs:121:20)
  │       #8: 0x5555703cec54: pingora_lru::LruUnit<T>::with_capacity (pingora-lru/src/lib.rs:176:20)
  │       #9: 0x5555703cec54: pingora_lru::Lru<T,_>::with_capacity (pingora-lru/src/lib.rs:28:36)
  │       #10: 0x5555703cec54: pingora_cache::eviction::lru::Manager<_>::with_capacity (src/eviction/lru.rs:22:17)
  │       #11: 0x5555703cec54: lru_memory::main (pingora-cache/benches/lru_memory.rs:74:19)
  │     }
  │   }
  ├── PP 1.3/5 {
  │     Total:     142,606,592 bytes (20.92%, 5,896,544.09/s) in 32 blocks (0%, 1.32/s), avg size 4,456,456 bytes, avg lifetime 22,056,252.88 µs (91.2% of program duration)
  │     Max:       142,606,592 bytes in 32 blocks, avg size 4,456,456 bytes
  │     At t-gmax: 142,606,592 bytes (25.06%) in 32 blocks (0%), avg size 4,456,456 bytes
  │     At t-end:  0 bytes (0%) in 0 blocks (0%), avg size 0 bytes
  │     Allocated at {
  │       #1: 0x5555703ceb64: alloc::alloc::alloc (alloc/src/alloc.rs:95:14)
  │       #2: 0x5555703ceb64: <hashbrown::raw::alloc::inner::Global as hashbrown::raw::alloc::inner::Allocator>::allocate (src/raw/alloc.rs:47:35)
  │       #3: 0x5555703ceb64: hashbrown::raw::alloc::inner::do_alloc (src/raw/alloc.rs:62:9)
  │       #4: 0x5555703ceb64: hashbrown::raw::RawTableInner<A>::new_uninitialized (src/raw/mod.rs:1080:38)
  │       #5: 0x5555703ceb64: hashbrown::raw::RawTableInner<A>::fallible_with_capacity (src/raw/mod.rs:1109:30)
  │       #6: 0x5555703ceb64: hashbrown::raw::RawTable<T,A>::fallible_with_capacity (src/raw/mod.rs:460:20)
  │       #7: 0x5555703ceb64: hashbrown::raw::RawTable<T,A>::with_capacity_in (src/raw/mod.rs:481:15)
  │       #8: 0x5555703ceb64: hashbrown::raw::RawTable<T>::with_capacity (src/raw/mod.rs:411:9)
  │       #9: 0x5555703ceb64: hashbrown::map::HashMap<K,V,S>::with_capacity_and_hasher (hashbrown-0.12.3/src/map.rs:422:20)
  │       #10: 0x5555703ceb64: hashbrown::map::HashMap<K,V>::with_capacity (hashbrown-0.12.3/src/map.rs:326:9)
  │       #11: 0x5555703ceb64: pingora_lru::LruUnit<T>::with_capacity (pingora-lru/src/lib.rs:175:27)
  │       #12: 0x5555703ceb64: pingora_lru::Lru<T,_>::with_capacity (pingora-lru/src/lib.rs:28:36)
  │       #13: 0x5555703ceb64: pingora_cache::eviction::lru::Manager<_>::with_capacity (src/eviction/lru.rs:22:17)
  │       #14: 0x5555703ceb64: lru_memory::main (pingora-cache/benches/lru_memory.rs:74:19)
  │     }
  │   }
*/
fn main() {
    let _profiler = dhat::Profiler::new_heap();
    let manager = Manager::<32>::with_capacity(ITEMS, ITEMS / 32);
    let unused_ttl = std::time::SystemTime::now();
    for i in 0..ITEMS {
        let item = CacheKey::new("", i.to_string(), "").to_compact();
        manager.admit(item, 1, unused_ttl);
    }
}
