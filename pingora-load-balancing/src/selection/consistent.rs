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

//! Consistent Hashing

use super::*;
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_ketama::{Bucket, Continuum};
use std::collections::HashMap;

/// Weighted Ketama consistent hashing
pub struct KetamaHashing {
    ring: Continuum,
    // TODO: update Ketama to just store this
    backends: HashMap<SocketAddr, Backend>,
}

impl BackendSelection for KetamaHashing {
    type Iter = OwnedNodeIterator;

    fn build(backends: &BTreeSet<Backend>) -> Self {
        let buckets: Vec<_> = backends
            .iter()
            .filter_map(|b| {
                // FIXME: ketama only supports Inet addr, UDS addrs are ignored here
                if let SocketAddr::Inet(addr) = b.addr {
                    Some(Bucket::new(addr, b.weight as u32))
                } else {
                    None
                }
            })
            .collect();
        let new_backends = backends
            .iter()
            .map(|b| (b.addr.clone(), b.clone()))
            .collect();
        KetamaHashing {
            ring: Continuum::new(&buckets),
            backends: new_backends,
        }
    }

    fn iter(self: &Arc<Self>, key: &[u8]) -> Self::Iter {
        OwnedNodeIterator {
            idx: self.ring.node_idx(key),
            ring: self.clone(),
        }
    }
}

/// Iterator over a Continuum
pub struct OwnedNodeIterator {
    idx: usize,
    ring: Arc<KetamaHashing>,
}

impl BackendIter for OwnedNodeIterator {
    fn next(&mut self) -> Option<&Backend> {
        self.ring.ring.get_addr(&mut self.idx).and_then(|addr| {
            let addr = SocketAddr::Inet(*addr);
            self.ring.backends.get(&addr)
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_ketama() {
        let b1 = Backend::new("1.1.1.1:80").unwrap();
        let b2 = Backend::new("1.0.0.1:80").unwrap();
        let b3 = Backend::new("1.0.0.255:80").unwrap();
        let backends = BTreeSet::from_iter([b1.clone(), b2.clone(), b3.clone()]);
        let hash = Arc::new(KetamaHashing::build(&backends));

        let mut iter = hash.iter(b"test0");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test2");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test3");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test4");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test5");
        assert_eq!(iter.next(), Some(&b3));
        let mut iter = hash.iter(b"test6");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test7");
        assert_eq!(iter.next(), Some(&b3));
        let mut iter = hash.iter(b"test8");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test9");
        assert_eq!(iter.next(), Some(&b2));

        // remove b3
        let backends = BTreeSet::from_iter([b1.clone(), b2.clone()]);
        let hash = Arc::new(KetamaHashing::build(&backends));
        let mut iter = hash.iter(b"test0");
        assert_eq!(iter.next(), Some(&b2));
        let mut iter = hash.iter(b"test1");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test2");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test3");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test4");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test5");
        assert_eq!(iter.next(), Some(&b2)); // changed
        let mut iter = hash.iter(b"test6");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test7");
        assert_eq!(iter.next(), Some(&b1)); // changed
        let mut iter = hash.iter(b"test8");
        assert_eq!(iter.next(), Some(&b1));
        let mut iter = hash.iter(b"test9");
        assert_eq!(iter.next(), Some(&b2));
    }
}
