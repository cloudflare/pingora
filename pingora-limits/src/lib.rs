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

//! The pingora_limits crate contains modules that can help introduce things like rate limiting or
//! thread-safe event count estimation.

#![warn(clippy::all)]
#![allow(clippy::new_without_default)]
#![allow(clippy::type_complexity)]

pub mod estimator;
pub mod inflight;
pub mod rate;

use ahash::RandomState;
use std::hash::Hash;

#[inline]
fn hash<T: Hash>(key: T, hasher: &RandomState) -> u64 {
    hasher.hash_one(key)
}
