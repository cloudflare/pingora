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
use old_version::{Bucket as OldBucket, Continuum as OldContinuum};
#[allow(unused_imports)]
use pingora_ketama::{Bucket, Continuum, Version, DEFAULT_POINT_MULTIPLE};
use rand::{random, random_range, rng, seq::IteratorRandom};
use std::collections::BTreeSet;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

mod old_version;

fn random_socket_addr() -> SocketAddr {
    if random::<bool>() {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from_bits(random()), random()))
    } else {
        SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from_bits(random()),
            random(),
            0,
            0,
        ))
    }
}

fn random_string(len: usize) -> String {
    const CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rng();
    (0..len)
        .map(|_| CHARS.chars().choose(&mut rng).unwrap())
        .collect()
}

/// The old version of pingora-ketama should _always_ return the same result as
/// v1 of the new version as long as the original input is sorted by by socket
/// address (and has no duplicates). this test generates a large number of
/// random socket addresses with varying weights and compares the output of
/// both
#[test]
fn test_v1_to_old_version() {
    let (old_buckets, new_buckets): (BTreeSet<_>, BTreeSet<_>) = (0..2000)
        .map(|_| (random_socket_addr(), random_range(1..10)))
        .map(|(addr, weight)| (OldBucket::new(addr, weight), Bucket::new(addr, weight)))
        .unzip();

    let old_continuum = OldContinuum::new(&Vec::from_iter(old_buckets));
    let new_continuum = Continuum::new(&Vec::from_iter(new_buckets));

    for _ in 0..20_000 {
        let key = random_string(20);
        let old_node = old_continuum.node(key.as_bytes()).unwrap();
        let new_node = new_continuum.node(key.as_bytes()).unwrap();

        assert_eq!(old_node, new_node);
    }
}

/// The new version of pingora-ketama (v2) should return _almost_ exactly what
/// the old version does. The difference will be in collision handling
#[test]
#[cfg(feature = "v2")]
fn test_v2_to_old_version() {
    let (old_buckets, new_buckets): (BTreeSet<_>, BTreeSet<_>) = (0..2000)
        .map(|_| (random_socket_addr(), random_range(1..10)))
        .map(|(addr, weight)| (OldBucket::new(addr, weight), Bucket::new(addr, weight)))
        .unzip();

    let old_continuum = OldContinuum::new(&Vec::from_iter(old_buckets));

    let new_continuum = Continuum::new_with_version(
        &Vec::from_iter(new_buckets),
        Version::V2 {
            point_multiple: DEFAULT_POINT_MULTIPLE,
        },
    );

    let test_count = 20_000;
    let mut mismatches = 0;

    for _ in 0..test_count {
        let key = random_string(20);
        let old_node = old_continuum.node(key.as_bytes()).unwrap();
        let new_node = new_continuum.node(key.as_bytes()).unwrap();

        if old_node != new_node {
            mismatches += 1;
        }
    }

    assert!((mismatches as f64 / test_count as f64) < 0.001);
}
