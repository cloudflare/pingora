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

//! Service discovery interface and implementations

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_error::Result;
use std::hash::Hash;
use std::io::Result as IoResult;
use std::net::ToSocketAddrs;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::Backend;

/// [ServiceDiscovery] is the interface to discover [Backend]s.
#[async_trait]
pub trait ServiceDiscovery {
    type Metadata;
    /// Return the discovered collection of backends.
    /// And *optionally* whether these backends are enabled to serve or not in a `HashMap`. Any backend
    /// that is not explicitly in the set is considered enabled.
    async fn discover(&self) -> Result<(HashSet<Backend<Self::Metadata>>, HashMap<u64, bool>)>;
}

// TODO: add DNS base discovery

/// A static collection of [Backend]s for service discovery.
#[derive(Default)]
pub struct Static<M> {
    backends: ArcSwap<HashSet<Backend<M>>>,
}

impl<M> Static<M>
where
    M: Clone + Eq + Hash,
{
    /// Create a new boxed [Static] from a given iterator of items that implements [ToSocketAddrs].
    pub fn try_from_iter<A, T: IntoIterator<Item = (A, M)>>(iter: T) -> IoResult<Box<Self>>
    where
        A: ToSocketAddrs,
    {
        let mut upstreams = HashSet::new();
        for (addrs, metadata) in iter.into_iter() {
            let addrs = addrs.to_socket_addrs()?.map(|addr| Backend {
                addr: SocketAddr::Inet(addr),
                weight: 1,
                metadata: metadata.clone(),
            });
            upstreams.extend(addrs);
        }
        Ok(Self::new(upstreams))
    }
}

impl<M> Static<M>
where
    M: Clone + Eq + Hash,
{
    /// Create a new boxed [Static] service discovery with the given backends.
    pub fn new(backends: HashSet<Backend<M>>) -> Box<Self> {
        Box::new(Static {
            backends: ArcSwap::new(Arc::new(backends)),
        })
    }

    /// return the collection to backends
    pub fn get(&self) -> HashSet<Backend<M>> {
        HashSet::clone(&self.backends.load())
    }

    // Concurrent set/add/remove might race with each other
    // TODO: use a queue to avoid racing

    // TODO: take an impl iter
    #[allow(dead_code)]
    pub(crate) fn set(&self, backends: HashSet<Backend<M>>) {
        self.backends.store(backends.into())
    }

    #[allow(dead_code)]
    pub(crate) fn add(&self, backend: Backend<M>) {
        let mut new = self.get();
        new.insert(backend);
        self.set(new)
    }

    #[allow(dead_code)]
    pub(crate) fn remove(&self, backend: &Backend<M>) {
        let mut new = self.get();
        new.remove(backend);
        self.set(new)
    }
}

#[async_trait]
impl<M> ServiceDiscovery for Static<M>
where
    M: Hash + Eq + Clone + Send + Sync + 'static,
{
    type Metadata = M;

    async fn discover(&self) -> Result<(HashSet<Backend<M>>, HashMap<u64, bool>)> {
        // no readiness
        let health = HashMap::new();
        Ok((self.get(), health))
    }
}
