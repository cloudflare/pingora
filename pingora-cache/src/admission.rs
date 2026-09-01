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

//! Admission policies for deciding whether cache misses should be stored.

#[cfg(test)]
use crate::key::CompactCacheKey;
use crate::CacheKey;
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::num::NonZeroU32;
#[cfg(test)]
use std::sync::Mutex;

/// The result of observing an absent cache key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Skip cache admission for this request.
    Defer {
        /// Number of observations reported by the policy.
        observed: u32,
    },
    /// Allow the request to proceed through the normal miss and fill path.
    Ready {
        /// Number of observations reported by the policy.
        observed: u32,
    },
}

impl Decision {
    /// Return the number of observations reported by the policy.
    pub fn observed(self) -> u32 {
        match self {
            Self::Defer { observed } | Self::Ready { observed } => observed,
        }
    }

    /// Whether admission should be deferred.
    pub fn is_deferred(self) -> bool {
        matches!(self, Self::Defer { .. })
    }
}

/// Policy invoked after storage reports that a cache key is absent.
pub trait AdmissionPolicy: Send + Sync {
    /// Observe an absent key and return a [`Decision`] for this request.
    ///
    /// This method runs synchronously in the asynchronous cache lookup hot path. Implementations
    /// must be fast and non-blocking, and must not panic.
    ///
    /// Policies that retain the key beyond this call may convert it to a compact or
    /// policy-specific representation.
    fn observe(&self, key: &CacheKey) -> Decision;
}

/// Simple configurable policy used to exercise admission plumbing in tests.
#[cfg(test)]
pub(crate) struct MinUsesAdmissionPolicy {
    min_uses: NonZeroU32,
    observations: Mutex<HashMap<CompactCacheKey, u32>>,
}

#[cfg(test)]
impl MinUsesAdmissionPolicy {
    pub(crate) fn new(min_uses: NonZeroU32) -> Self {
        Self {
            min_uses,
            observations: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
impl AdmissionPolicy for MinUsesAdmissionPolicy {
    fn observe(&self, key: &CacheKey) -> Decision {
        let mut observations = self.observations.lock().unwrap();
        let observed = observations
            .entry(key.to_compact())
            .and_modify(|observed| *observed = observed.saturating_add(1).min(self.min_uses.get()))
            .or_insert(1);
        if *observed >= self.min_uses.get() {
            Decision::Ready {
                observed: *observed,
            }
        } else {
            Decision::Defer {
                observed: *observed,
            }
        }
    }
}
