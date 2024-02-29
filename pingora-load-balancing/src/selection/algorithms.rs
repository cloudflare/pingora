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

//! Implementation of algorithms for weighted selection
//!
//! All [std::hash::Hasher] + [Default] can be used directly as a selection algorithm.

use super::*;
use std::hash::Hasher;
use std::sync::atomic::{AtomicUsize, Ordering};

impl<H> SelectionAlgorithm for H
where
    H: Default + Hasher,
{
    fn new() -> Self {
        H::default()
    }
    fn next(&self, key: &[u8]) -> u64 {
        let mut hasher = H::default();
        hasher.write(key);
        hasher.finish()
    }
}

/// Round Robin selection
pub struct RoundRobin(AtomicUsize);

impl SelectionAlgorithm for RoundRobin {
    fn new() -> Self {
        Self(AtomicUsize::new(0))
    }
    fn next(&self, _key: &[u8]) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) as u64
    }
}

/// Random selection
pub struct Random;

impl SelectionAlgorithm for Random {
    fn new() -> Self {
        Self
    }
    fn next(&self, _key: &[u8]) -> u64 {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        rng.gen()
    }
}
