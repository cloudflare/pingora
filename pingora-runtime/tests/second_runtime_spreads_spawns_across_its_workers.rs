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

//! A second no-steal runtime must spread spawns across all of its workers.
//!
//! The first runtime shuts down, then a task on the second one spawns
//! through `current_handle()` many times. Every spawn must run, and they
//! must land on both of the second runtime's workers. If the first
//! runtime's registration survived in a recycled slot, as it does with a
//! `thread_local::ThreadLocal`, one worker would spawn onto the dead
//! runtime, or fall back to only its own runtime.
//!
//! This is a test file of its own with exactly one `#[test]`, for the
//! same reason as `no_steal_worker_spawns_onto_its_own_runtime.rs`:
//! another test thread would compete for the recycled ids.

use pingora_runtime::{current_handle, Runtime};
use std::collections::HashSet;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn second_runtime_spreads_spawns_across_its_workers() {
    let first = Runtime::new_no_steal(2, "first");
    let _ = first.get_handle();
    first.shutdown_timeout(Duration::from_secs(10));

    let second = Runtime::new_no_steal(2, "second");
    let (tx, rx) = mpsc::channel();
    second.get_handle().spawn(async move {
        let mut ids = HashSet::new();
        for _ in 0..64 {
            match current_handle()
                .spawn(async { std::thread::current().id() })
                .await
            {
                Ok(id) => {
                    ids.insert(id);
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            }
        }
        let _ = tx.send(Ok(ids.len()));
    });
    let r = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(
        r,
        Ok(2),
        "spawns must reach both workers of the live runtime"
    );
}
