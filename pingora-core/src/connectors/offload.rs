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

use log::debug;
use once_cell::sync::OnceCell;
use rand::Rng;
use tokio::runtime::{Builder, Handle};
use tokio::sync::oneshot::{channel, Sender};

// TODO: use pingora_runtime
// a shared runtime (thread pools)
pub(crate) struct OffloadRuntime {
    shards: usize,
    thread_per_shard: usize,
    // Lazily init the runtimes so that they are created after pingora
    // daemonize itself. Otherwise the runtime threads are lost.
    pools: OnceCell<Box<[(Handle, Sender<()>)]>>,
}

impl OffloadRuntime {
    pub fn new(shards: usize, thread_per_shard: usize) -> Self {
        assert!(shards != 0);
        assert!(thread_per_shard != 0);
        OffloadRuntime {
            shards,
            thread_per_shard,
            pools: OnceCell::new(),
        }
    }

    fn init_pools(&self) -> Box<[(Handle, Sender<()>)]> {
        let threads = self.shards * self.thread_per_shard;
        let mut pools = Vec::with_capacity(threads);
        for _ in 0..threads {
            // We use single thread runtimes to reduce the scheduling overhead of multithread
            // tokio runtime, which can be 50% of the on CPU time of the runtimes
            let rt = Builder::new_current_thread().enable_all().build().unwrap();
            let handler = rt.handle().clone();
            let (tx, rx) = channel::<()>();
            std::thread::Builder::new()
                .name("Offload thread".to_string())
                .spawn(move || {
                    debug!("Offload thread started");
                    // the thread that calls block_on() will drive the runtime
                    // rx will return when tx is dropped so this runtime and thread will exit
                    rt.block_on(rx)
                })
                .unwrap();
            pools.push((handler, tx));
        }

        pools.into_boxed_slice()
    }

    pub fn get_runtime(&self, hash: u64) -> &Handle {
        let mut rng = rand::thread_rng();

        // choose a shard based on hash and a random thread with in that shard
        // e.g. say thread_per_shard=2, shard 1 thread 1 is 1 * 2 + 1 = 3
        // [[th0, th1], [th2, th3], ...]
        let shard = hash as usize % self.shards;
        let thread_in_shard = rng.gen_range(0..self.thread_per_shard);
        let pools = self.pools.get_or_init(|| self.init_pools());
        &pools[shard * self.thread_per_shard + thread_in_shard].0
    }
}
