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

//! A fresh no-steal worker must not inherit a shut-down runtime.
//!
//! `CURRENT_HANDLE` is a `thread_local::ThreadLocal`. Each
//! `NoStealRuntime` worker registers its own runtime's pools there in
//! `init_pools()`, and `current_handle()` reads them back. The map is
//! keyed by an id the `thread_local` crate allocates from a free list
//! and recycles the moment a thread exits, so a registration outlives
//! the thread that made it and reappears under whichever thread is
//! handed the same id next.
//!
//! When that next thread is a worker of a newer `NoStealRuntime`, it
//! finds the old entry under its id. Without the owning `ThreadId`
//! stored beside the pools, the new worker holds a handle to the runtime
//! that already shut down, and every task it spawns through
//! `current_handle()` is canceled on arrival, on a runtime that is
//! healthy and has nothing wrong with it.
//!
//! # Why it is deterministic
//!
//! Not timing, and no sleeps. It rests on being the only thing in its
//! process that allocates a `thread_local` id, which is why this is a
//! test file of its own with exactly one `#[test]` in it:
//!
//! 1. The first runtime's workers are the first threads in the process
//!    to ask for a `thread_local` id, so they take the lowest ids, id 0
//!    among them, and each leaves a registration under the id it took.
//! 2. `shutdown_timeout` joins those threads, which is what returns
//!    their ids to the free list. The registrations stay where they are.
//! 3. The free list is a `BinaryHeap<Reverse<usize>>` and pops the
//!    lowest id first, so the next thread to ask is handed one of them.
//! 4. The second runtime is built with a single worker, so that one
//!    worker is the next thread to ask, and the single handle
//!    `get_handle()` can return is that worker's.
//!
//! Add a second `#[test]` to this file and libtest will run it on
//! another thread that competes for the same ids, and step 4 stops
//! holding. Anything else to cover belongs in its own file.

use std::sync::mpsc;
use std::time::Duration;

use pingora_runtime::{current_handle, Runtime};

/// The runtime that shuts down. Any thread count works: whatever ids its
/// workers take, id 0 is one of them, and every id they take is left
/// pointing at this runtime's pools.
const FIRST_THREADS: usize = 2;

/// The runtime that outlives it, with exactly one worker, so the single
/// handle `get_handle()` can return belongs to the thread that was given
/// the lowest recycled id.
const SECOND_THREADS: usize = 1;

#[test]
fn no_steal_worker_spawns_onto_its_own_runtime() {
    // A no-steal runtime, used and then shut down. Reading the handle is
    // what builds the pools and spawns the worker threads, and each
    // worker registers this runtime's pools against its own thread id
    // before it starts driving its runtime.
    let first = Runtime::new_no_steal(FIRST_THREADS, "first");
    let _ = first.get_handle();
    // Joins the worker threads. That is what puts their thread ids back
    // on the free list. The registrations they left behind stay.
    first.shutdown_timeout(Duration::from_secs(10));

    // A second no-steal runtime, built after the first one is gone. Its
    // worker is handed a recycled id, and with it the first runtime's
    // registration.
    let second = Runtime::new_no_steal(SECOND_THREADS, "second");

    // Ask the worker of the second runtime, from a task running on it,
    // to spawn through the public entry point. The second runtime is
    // alive and idle, so the task has to run.
    let (tx, rx) = mpsc::channel();
    second.get_handle().spawn(async move {
        let spawned = current_handle().spawn(async { 7u32 }).await;
        let _ = tx.send(spawned.map_err(|e| e.to_string()));
    });
    let outcome = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the worker of the second runtime polls the probe task");

    match outcome {
        Ok(value) => assert_eq!(value, 7, "the probe task returns its own value"),
        // Observed before the fix: "task 2 was cancelled".
        Err(err) => panic!(
            "a worker of the live second runtime spawned onto the shut-down first one: {err}"
        ),
    }
}
