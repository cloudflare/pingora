// Copyright 2025 Cloudflare, Inc.
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

//! This mod is a direct copy of the old version of pingora-ketama. It is here
//! to ensure that the new version's compatible mode is produces identical
//! results as the old version.

use std::cmp::Ordering;
use std::io::Write;
use std::net::SocketAddr;

use crc32fast::Hasher;

/// A [Bucket] represents a server for consistent hashing
///
/// A [Bucket] contains a [SocketAddr] to the server and a weight associated with it.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct Bucket {
    // The node name.
    // TODO: UDS
    node: SocketAddr,

    // The weight associated with a node. A higher weight indicates that this node should
    // receive more requests.
    weight: u32,
}

impl Bucket {
    /// Return a new bucket with the given node and weight.
    ///
    /// The chance that a [Bucket] is selected is proportional to the relative weight of all [Bucket]s.
    ///
    /// # Panics
    ///
    /// This will panic if the weight is zero.
    pub fn new(node: SocketAddr, weight: u32) -> Self {
        assert!(weight != 0, "weight must be at least one");

        Bucket { node, weight }
    }
}

// A point on the continuum.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Point {
    // the index to the actual address
    node: u32,
    hash: u32,
}

// We only want to compare the hash when sorting, so we implement these traits by hand.
impl Ord for Point {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hash.cmp(&other.hash)
    }
}

impl PartialOrd for Point {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Point {
    fn new(node: u32, hash: u32) -> Self {
        Point { node, hash }
    }
}

/// The consistent hashing ring
///
/// A [Continuum] represents a ring of buckets where a node is associated with various points on
/// the ring.
pub struct Continuum {
    ring: Box<[Point]>,
    addrs: Box<[SocketAddr]>,
}

impl Continuum {
    /// Create a new [Continuum] with the given list of buckets.
    pub fn new(buckets: &[Bucket]) -> Self {
        // This constant is copied from nginx. It will create 160 points per weight unit. For
        // example, a weight of 2 will create 320 points on the ring.
        const POINT_MULTIPLE: u32 = 160;

        if buckets.is_empty() {
            return Continuum {
                ring: Box::new([]),
                addrs: Box::new([]),
            };
        }

        // The total weight is multiplied by the factor of points to create many points per node.
        let total_weight: u32 = buckets.iter().fold(0, |sum, b| sum + b.weight);
        let mut ring = Vec::with_capacity((total_weight * POINT_MULTIPLE) as usize);
        let mut addrs = Vec::with_capacity(buckets.len());

        for bucket in buckets {
            let mut hasher = Hasher::new();

            // We only do the following for backwards compatibility with nginx/memcache:
            // - Convert SocketAddr to string
            // - The hash input is as follows "HOST EMPTY PORT PREVIOUS_HASH". Spaces are only added
            //   for readability.
            // TODO: remove this logic and hash the literal SocketAddr once we no longer
            // need backwards compatibility

            // with_capacity = max_len(ipv6)(39) + len(null)(1) + max_len(port)(5)
            let mut hash_bytes = Vec::with_capacity(39 + 1 + 5);
            write!(&mut hash_bytes, "{}", bucket.node.ip()).unwrap();
            write!(&mut hash_bytes, "\0").unwrap();
            write!(&mut hash_bytes, "{}", bucket.node.port()).unwrap();
            hasher.update(hash_bytes.as_ref());

            // A higher weight will add more points for this node.
            let num_points = bucket.weight * POINT_MULTIPLE;

            // This is appended to the crc32 hash for each point.
            let mut prev_hash: u32 = 0;
            addrs.push(bucket.node);
            let node = addrs.len() - 1;
            for _ in 0..num_points {
                let mut hasher = hasher.clone();
                hasher.update(&prev_hash.to_le_bytes());

                let hash = hasher.finalize();
                ring.push(Point::new(node as u32, hash));
                prev_hash = hash;
            }
        }

        // Sort and remove any duplicates.
        ring.sort_unstable();
        ring.dedup_by(|a, b| a.hash == b.hash);

        Continuum {
            ring: ring.into_boxed_slice(),
            addrs: addrs.into_boxed_slice(),
        }
    }

    /// Find the associated index for the given input.
    pub fn node_idx(&self, input: &[u8]) -> usize {
        let hash = crc32fast::hash(input);

        // The `Result` returned here is either a match or the error variant returns where the
        // value would be inserted.
        match self.ring.binary_search_by(|p| p.hash.cmp(&hash)) {
            Ok(i) => i,
            Err(i) => {
                // We wrap around to the front if this value would be inserted at the end.
                if i == self.ring.len() {
                    0
                } else {
                    i
                }
            }
        }
    }

    /// Hash the given `hash_key` to the server address.
    pub fn node(&self, hash_key: &[u8]) -> Option<SocketAddr> {
        self.ring
            .get(self.node_idx(hash_key)) // should we unwrap here?
            .map(|p| self.addrs[p.node as usize])
    }
}
