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

//! Generic connection pooling
//!
//! The pool is optimized for high concurrency, high RPS use cases. Each connection group has a
//! lock free hot pool to reduce the lock contention when some connections are reused and released
//! very frequently.

#![warn(clippy::all)]
#![allow(clippy::new_without_default)]
#![allow(clippy::type_complexity)]

mod connection;
mod lru;

pub use connection::{ConnectionMeta, ConnectionPool, PoolNode};
