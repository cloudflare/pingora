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

//! # pingora-ketama
//! A Rust port of the nginx consistent hashing algorithm.
//!
//! This crate provides a consistent hashing algorithm which is identical in
//! behavior to [nginx consistent hashing](https://www.nginx.com/resources/wiki/modules/consistent_hash/).
//!
//! Using a consistent hash strategy like this is useful when one wants to
//! minimize the amount of requests that need to be rehashed to different nodes
//! when a node is added or removed.
//!
//! Here's a simple example of how one might use it:
//!
//! ```
//! use pingora_ketama::{Bucket, Continuum};
//!
//! # #[allow(clippy::needless_doctest_main)]
//! fn main() {
//!     // Set up a continuum with a few nodes of various weight.
//!     let mut buckets = vec![];
//!     buckets.push(Bucket::new("127.0.0.1:12345".parse().unwrap(), 1));
//!     buckets.push(Bucket::new("127.0.0.2:12345".parse().unwrap(), 2));
//!     buckets.push(Bucket::new("127.0.0.3:12345".parse().unwrap(), 3));
//!     let ring = Continuum::new(&buckets);
//!
//!     // Let's see what the result is for a few keys:
//!     for key in &["some_key", "another_key", "last_key"] {
//!         let node = ring.node(key.as_bytes()).unwrap();
//!         println!("{}: {}:{}", key, node.ip(), node.port());
//!     }
//! }
//! ```
//!
//! ```bash
//! # Output:
//! some_key: 127.0.0.3:12345
//! another_key: 127.0.0.3:12345
//! last_key: 127.0.0.2:12345
//! ```
//!
//! We've provided a health-aware example in
//! `pingora-ketama/examples/health_aware_selector.rs`.
//!
//! For a carefully crafted real-world example, see the [`pingora-load-balancing`](https://docs.rs/pingora-load-balancing)
//! crate.

use std::cmp::Ordering;
use std::io::Write;
use std::net::SocketAddr;

use crc32fast::Hasher;

/// A [Bucket] represents a server for consistent hashing
///
/// A [Bucket] contains a [SocketAddr] to the server and a weight associated with it.
#[derive(Clone, Debug, Eq, PartialEq, PartialOrd)]
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

    /// Get an iterator of nodes starting at the original hashed node of the `hash_key`.
    ///
    /// This function is useful to find failover servers if the original ones are offline, which is
    /// cheaper than rebuilding the entire hash ring.
    pub fn node_iter(&self, hash_key: &[u8]) -> NodeIterator {
        NodeIterator {
            idx: self.node_idx(hash_key),
            continuum: self,
        }
    }

    pub fn get_addr(&self, idx: &mut usize) -> Option<&SocketAddr> {
        let point = self.ring.get(*idx);
        if point.is_some() {
            // only update idx for non-empty ring otherwise we will panic on modulo 0
            *idx = (*idx + 1) % self.ring.len();
        }
        point.map(|p| &self.addrs[p.node as usize])
    }
}

/// Iterator over a Continuum
pub struct NodeIterator<'a> {
    idx: usize,
    continuum: &'a Continuum,
}

impl<'a> Iterator for NodeIterator<'a> {
    type Item = &'a SocketAddr;

    fn next(&mut self) -> Option<Self::Item> {
        self.continuum.get_addr(&mut self.idx)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::Path;

    use super::{Bucket, Continuum};

    fn get_sockaddr(ip: &str) -> SocketAddr {
        ip.parse().unwrap()
    }

    #[test]
    fn consistency_after_adding_host() {
        fn assert_hosts(c: &Continuum) {
            assert_eq!(c.node(b"a"), Some(get_sockaddr("127.0.0.10:6443")));
            assert_eq!(c.node(b"b"), Some(get_sockaddr("127.0.0.5:6443")));
        }

        let buckets: Vec<_> = (1..11)
            .map(|u| Bucket::new(get_sockaddr(&format!("127.0.0.{u}:6443")), 1))
            .collect();
        let c = Continuum::new(&buckets);
        assert_hosts(&c);

        // Now add a new host and ensure that the hosts don't get shuffled.
        let buckets: Vec<_> = (1..12)
            .map(|u| Bucket::new(get_sockaddr(&format!("127.0.0.{u}:6443")), 1))
            .collect();

        let c = Continuum::new(&buckets);
        assert_hosts(&c);
    }

    #[test]
    fn matches_nginx_sample() {
        let upstream_hosts = ["127.0.0.1:7777", "127.0.0.1:7778"];
        let upstream_hosts = upstream_hosts.iter().map(|i| get_sockaddr(i));

        let mut buckets = Vec::new();
        for upstream in upstream_hosts {
            buckets.push(Bucket::new(upstream, 1));
        }

        let c = Continuum::new(&buckets);

        assert_eq!(c.node(b"/some/path"), Some(get_sockaddr("127.0.0.1:7778")));
        assert_eq!(
            c.node(b"/some/longer/path"),
            Some(get_sockaddr("127.0.0.1:7777"))
        );
        assert_eq!(
            c.node(b"/sad/zaidoon"),
            Some(get_sockaddr("127.0.0.1:7778"))
        );
        assert_eq!(c.node(b"/g"), Some(get_sockaddr("127.0.0.1:7777")));
        assert_eq!(
            c.node(b"/pingora/team/is/cool/and/this/is/a/long/uri"),
            Some(get_sockaddr("127.0.0.1:7778"))
        );
        assert_eq!(
            c.node(b"/i/am/not/confident/in/this/code"),
            Some(get_sockaddr("127.0.0.1:7777"))
        );
    }

    #[test]
    fn matches_nginx_sample_data() {
        let upstream_hosts = [
            "10.0.0.1:443",
            "10.0.0.2:443",
            "10.0.0.3:443",
            "10.0.0.4:443",
            "10.0.0.5:443",
            "10.0.0.6:443",
            "10.0.0.7:443",
            "10.0.0.8:443",
            "10.0.0.9:443",
        ];
        let upstream_hosts = upstream_hosts.iter().map(|i| get_sockaddr(i));

        let mut buckets = Vec::new();
        for upstream in upstream_hosts {
            buckets.push(Bucket::new(upstream, 100));
        }

        let c = Continuum::new(&buckets);

        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join("sample-nginx-upstream.csv");

        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(path)
            .unwrap();

        for pair in rdr.records() {
            let pair = pair.unwrap();
            let uri = pair.get(0).unwrap();
            let upstream = pair.get(1).unwrap();

            let got = c.node(uri.as_bytes()).unwrap();
            assert_eq!(got, get_sockaddr(upstream));
        }
    }

    #[test]
    fn node_iter() {
        let upstream_hosts = ["127.0.0.1:7777", "127.0.0.1:7778", "127.0.0.1:7779"];
        let upstream_hosts = upstream_hosts.iter().map(|i| get_sockaddr(i));

        let mut buckets = Vec::new();
        for upstream in upstream_hosts {
            buckets.push(Bucket::new(upstream, 1));
        }

        let c = Continuum::new(&buckets);
        let mut iter = c.node_iter(b"doghash");
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7778")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7779")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7779")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7777")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7777")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7778")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7778")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7779")));

        // drop 127.0.0.1:7777
        let upstream_hosts = ["127.0.0.1:7777", "127.0.0.1:7779"];
        let upstream_hosts = upstream_hosts.iter().map(|i| get_sockaddr(i));

        let mut buckets = Vec::new();
        for upstream in upstream_hosts {
            buckets.push(Bucket::new(upstream, 1));
        }

        let c = Continuum::new(&buckets);
        let mut iter = c.node_iter(b"doghash");
        // 127.0.0.1:7778 nodes are gone now
        // assert_eq!(iter.next(), Some("127.0.0.1:7778"));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7779")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7779")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7777")));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7777")));
        // assert_eq!(iter.next(), Some("127.0.0.1:7778"));
        // assert_eq!(iter.next(), Some("127.0.0.1:7778"));
        assert_eq!(iter.next(), Some(&get_sockaddr("127.0.0.1:7779")));

        // assert infinite cycle
        let c = Continuum::new(&[Bucket::new(get_sockaddr("127.0.0.1:7777"), 1)]);
        let mut iter = c.node_iter(b"doghash");

        let start_idx = iter.idx;
        for _ in 0..c.ring.len() {
            assert!(iter.next().is_some());
        }
        // assert wrap around
        assert_eq!(start_idx, iter.idx);
    }

    #[test]
    fn test_empty() {
        let c = Continuum::new(&[]);
        assert!(c.node(b"doghash").is_none());

        let mut iter = c.node_iter(b"doghash");
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_ipv6_ring() {
        let upstream_hosts = ["[::1]:7777", "[::1]:7778", "[::1]:7779"];
        let upstream_hosts = upstream_hosts.iter().map(|i| get_sockaddr(i));

        let mut buckets = Vec::new();
        for upstream in upstream_hosts {
            buckets.push(Bucket::new(upstream, 1));
        }

        let c = Continuum::new(&buckets);
        let mut iter = c.node_iter(b"doghash");
        assert_eq!(iter.next(), Some(&get_sockaddr("[::1]:7777")));
        assert_eq!(iter.next(), Some(&get_sockaddr("[::1]:7778")));
        assert_eq!(iter.next(), Some(&get_sockaddr("[::1]:7777")));
        assert_eq!(iter.next(), Some(&get_sockaddr("[::1]:7778")));
        assert_eq!(iter.next(), Some(&get_sockaddr("[::1]:7778")));
        assert_eq!(iter.next(), Some(&get_sockaddr("[::1]:7777")));
        assert_eq!(iter.next(), Some(&get_sockaddr("[::1]:7779")));
    }
}
