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

use std::time::Instant;

use pingora_cache::{
    eviction::{lru::Manager, EvictionManager},
    CacheKey,
};

const ITEMS: usize = 5 * usize::pow(2, 20);

fn main() {
    let manager = Manager::<32>::with_capacity(ITEMS, ITEMS / 32);
    let manager2 = Manager::<32>::with_capacity(ITEMS, ITEMS / 32);
    let unused_ttl = std::time::SystemTime::now();
    for i in 0..ITEMS {
        let item = CacheKey::new("", i.to_string(), "").to_compact();
        manager.admit(item, 1, unused_ttl);
    }

    /* lru serialize shard 19 22.573338ms, 5241623 bytes
     * lru deserialize shard 19 39.260669ms, 5241623 bytes */
    for i in 0..32 {
        let before = Instant::now();
        let ser = manager.serialize_shard(i).unwrap();
        let elapsed = before.elapsed();
        println!("lru serialize shard {i} {elapsed:?}, {} bytes", ser.len());

        let before = Instant::now();
        manager2.deserialize_shard(&ser).unwrap();
        let elapsed = before.elapsed();
        println!("lru deserialize shard {i} {elapsed:?}, {} bytes", ser.len());
    }
}
