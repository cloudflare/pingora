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

//! A thread that is not a no-steal worker must not get a dead runtime.
//!
//! The first runtime shuts down, then a plain thread running its own
//! runtime calls `current_handle()`. It must get its own runtime. If the
//! first runtime's registration survived in a recycled slot, as it does
//! with a `thread_local::ThreadLocal`, the thread would be handed the
//! dead runtime and its spawn would be canceled.
//!
//! This is a test file of its own with exactly one `#[test]`, for the
//! same reason as `no_steal_worker_spawns_onto_its_own_runtime.rs`:
//! another test thread would compete for the recycled ids.

use pingora_runtime::{current_handle, Runtime};
use std::time::Duration;
use thread_local::ThreadLocal;

#[test]
fn non_worker_thread_does_not_get_dead_runtime() {
    let first = Runtime::new_no_steal(2, "first");
    let _ = first.get_handle();
    first.shutdown_timeout(Duration::from_secs(10));

    let out = std::thread::spawn(|| {
        // Allocate a `thread_local` id explicitly, whether or not
        // `ThreadLocal::get()` would: this thread is handed the lowest free
        // one, which a worker of the first runtime has just released.
        let other: ThreadLocal<u8> = ThreadLocal::new();
        let _ = other.get_or(|| 0);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            current_handle()
                .spawn(async { 7u32 })
                .await
                .map_err(|e| e.to_string())
        })
    })
    .join()
    .unwrap();
    assert_eq!(out, Ok(7));
}
