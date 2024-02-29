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

fn main() {
    const ITEMS: usize = 5_000_000;

    // push bench

    let mut std_list = std::collections::LinkedList::<u64>::new();
    let before = Instant::now();
    for _ in 0..ITEMS {
        std_list.push_front(0);
    }
    let elapsed = before.elapsed();
    println!(
        "std linked list push_front total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    let mut list = pingora_lru::linked_list::LinkedList::with_capacity(ITEMS);
    let before = Instant::now();
    for _ in 0..ITEMS {
        list.push_head(0);
    }
    let elapsed = before.elapsed();
    println!(
        "pingora linked list push_head total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    // iter bench

    let mut count = 0;
    let before = Instant::now();
    for _ in std_list.iter() {
        count += 1;
    }
    let elapsed = before.elapsed();
    println!(
        "std linked list iter total {count} {elapsed:?}, {:?} avg per operation",
        elapsed / count as u32
    );

    let mut count = 0;
    let before = Instant::now();
    for _ in list.iter() {
        count += 1;
    }
    let elapsed = before.elapsed();
    println!(
        "pingora linked list iter total {count} {elapsed:?}, {:?} avg per operation",
        elapsed / count as u32
    );

    // search bench

    let before = Instant::now();
    for _ in 0..ITEMS {
        assert!(!std_list.iter().take(10).any(|v| *v == 1));
    }
    let elapsed = before.elapsed();
    println!(
        "std linked search first 10 items total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    let before = Instant::now();
    for _ in 0..ITEMS {
        assert!(!list.iter().take(10).any(|v| *v == 1));
    }
    let elapsed = before.elapsed();
    println!(
        "pingora linked search first 10 items total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    let before = Instant::now();
    for _ in 0..ITEMS {
        assert!(!list.exist_near_head(1, 10));
    }
    let elapsed = before.elapsed();
    println!(
        "pingora linked optimized search first 10 items total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    // move node bench
    let before = Instant::now();
    for _ in 0..ITEMS {
        let value = std_list.pop_back().unwrap();
        std_list.push_front(value);
    }
    let elapsed = before.elapsed();
    println!(
        "std linked list move back to front total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    let before = Instant::now();
    for _ in 0..ITEMS {
        let index = list.tail().unwrap();
        list.promote(index);
    }
    let elapsed = before.elapsed();
    println!(
        "pingora linked list move tail to head total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    // pop bench

    let before = Instant::now();
    for _ in 0..ITEMS {
        std_list.pop_back();
    }
    let elapsed = before.elapsed();
    println!(
        "std linked list pop_back {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );

    let before = Instant::now();
    for _ in 0..ITEMS {
        list.pop_tail();
    }
    let elapsed = before.elapsed();
    println!(
        "pingora linked list pop_tail total {elapsed:?}, {:?} avg per operation",
        elapsed / ITEMS as u32
    );
}
