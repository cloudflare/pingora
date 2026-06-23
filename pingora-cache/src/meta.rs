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

//! Metadata for caching

pub use http::Extensions;
use log::warn;
use once_cell::sync::{Lazy, OnceCell};
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use pingora_header_serde::HeaderSerde;
use pingora_http::{HMap, ResponseHeader};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use crate::key::HashBinary;

pub(crate) type InternalMeta = internal_meta::InternalMetaLatest;
mod internal_meta {
    use super::*;

    pub(crate) type InternalMetaLatest = InternalMetaV2;

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub(crate) struct InternalMetaV0 {
        pub(crate) fresh_until: SystemTime,
        pub(crate) created: SystemTime,
        pub(crate) stale_while_revalidate_sec: u32,
        pub(crate) stale_if_error_sec: u32,
        // Do not add more field
    }

    impl InternalMetaV0 {
        #[allow(dead_code)]
        fn serialize(&self) -> Result<Vec<u8>> {
            rmp_serde::encode::to_vec(self).or_err(InternalError, "failed to encode cache meta")
        }

        fn deserialize(buf: &[u8]) -> Result<Self> {
            rmp_serde::decode::from_slice(buf)
                .or_err(InternalError, "failed to decode cache meta v0")
        }
    }

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub(crate) struct InternalMetaV1 {
        pub(crate) version: u8,
        pub(crate) fresh_until: SystemTime,
        pub(crate) created: SystemTime,
        pub(crate) stale_while_revalidate_sec: u32,
        pub(crate) stale_if_error_sec: u32,
        // Do not add more field
    }

    impl InternalMetaV1 {
        #[allow(dead_code)]
        pub const VERSION: u8 = 1;

        #[allow(dead_code)]
        pub fn serialize(&self) -> Result<Vec<u8>> {
            assert_eq!(self.version, 1);
            rmp_serde::encode::to_vec(self).or_err(InternalError, "failed to encode cache meta")
        }

        fn deserialize(buf: &[u8]) -> Result<Self> {
            rmp_serde::decode::from_slice(buf)
                .or_err(InternalError, "failed to decode cache meta v1")
        }
    }

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub(crate) struct InternalMetaV2 {
        pub(crate) version: u8,
        pub(crate) fresh_until: SystemTime,
        pub(crate) created: SystemTime,
        pub(crate) updated: SystemTime,
        pub(crate) stale_while_revalidate_sec: u32,
        pub(crate) stale_if_error_sec: u32,
        // Only the extended field to be added below. One field at a time.
        // 1. serde default in order to accept an older version schema without the field existing
        // 2. serde skip_serializing_if in order for software with only an older version of this
        //    schema to decode it
        // After full releases, remove `skip_serializing_if` so that we can add the next extended field.
        #[serde(default)]
        pub(crate) variance: Option<HashBinary>,
        #[serde(default)]
        pub(crate) epoch_override: Option<SystemTime>,
        // Cache-object provenance timestamp for hit filtering decisions that need a
        // stable reference point across metadata rewrites or refreshes.
        #[serde(default)]
        #[serde(skip_serializing_if = "Option::is_none")]
        pub(crate) provenance: Option<SystemTime>,
    }

    impl Default for InternalMetaV2 {
        fn default() -> Self {
            let epoch = SystemTime::UNIX_EPOCH;
            InternalMetaV2 {
                version: InternalMetaV2::VERSION,
                fresh_until: epoch,
                created: epoch,
                updated: epoch,
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
                variance: None,
                epoch_override: None,
                provenance: None,
            }
        }
    }

    impl InternalMetaV2 {
        pub const VERSION: u8 = 2;

        pub fn serialize(&self) -> Result<Vec<u8>> {
            assert_eq!(self.version, Self::VERSION);
            rmp_serde::encode::to_vec(self).or_err(InternalError, "failed to encode cache meta")
        }

        fn deserialize(buf: &[u8]) -> Result<Self> {
            rmp_serde::decode::from_slice(buf)
                .or_err(InternalError, "failed to decode cache meta v2")
        }
    }

    impl From<InternalMetaV0> for InternalMetaV2 {
        fn from(v0: InternalMetaV0) -> Self {
            InternalMetaV2 {
                version: InternalMetaV2::VERSION,
                fresh_until: v0.fresh_until,
                created: v0.created,
                updated: v0.created,
                stale_while_revalidate_sec: v0.stale_while_revalidate_sec,
                stale_if_error_sec: v0.stale_if_error_sec,
                ..Default::default()
            }
        }
    }

    impl From<InternalMetaV1> for InternalMetaV2 {
        fn from(v1: InternalMetaV1) -> Self {
            InternalMetaV2 {
                version: InternalMetaV2::VERSION,
                fresh_until: v1.fresh_until,
                created: v1.created,
                updated: v1.created,
                stale_while_revalidate_sec: v1.stale_while_revalidate_sec,
                stale_if_error_sec: v1.stale_if_error_sec,
                ..Default::default()
            }
        }
    }

    // cross version decode
    pub(crate) fn deserialize(buf: &[u8]) -> Result<InternalMetaLatest> {
        const MIN_SIZE: usize = 10; // a small number to read the first few bytes
        if buf.len() < MIN_SIZE {
            return Error::e_explain(
                InternalError,
                format!("Buf too short ({}) to be InternalMeta", buf.len()),
            );
        }
        let preread_buf = &mut &buf[..MIN_SIZE];
        // the struct is always packed as a fixed size array
        match rmp::decode::read_array_len(preread_buf)
            .or_err(InternalError, "failed to decode cache meta array size")?
        {
            // v0 has 4 items and no version number
            4 => Ok(InternalMetaV0::deserialize(buf)?.into()),
            // other V should have version number encoded
            _ => {
                // rmp will encode `version` < 128 into a fixint (one byte),
                // so we use read_pfix
                let version = rmp::decode::read_pfix(preread_buf)
                    .or_err(InternalError, "failed to decode meta version")?;
                match version {
                    1 => Ok(InternalMetaV1::deserialize(buf)?.into()),
                    2 => InternalMetaV2::deserialize(buf),
                    _ => Error::e_explain(
                        InternalError,
                        format!("Unknown InternalMeta version {version}"),
                    ),
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_internal_meta_serde_v0() {
            let meta = InternalMetaV0 {
                fresh_until: SystemTime::now(),
                created: SystemTime::now(),
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
            };
            let binary = meta.serialize().unwrap();
            let meta2 = InternalMetaV0::deserialize(&binary).unwrap();
            assert_eq!(meta.fresh_until, meta2.fresh_until);
        }

        #[test]
        fn test_internal_meta_serde_v1() {
            let meta = InternalMetaV1 {
                version: InternalMetaV1::VERSION,
                fresh_until: SystemTime::now(),
                created: SystemTime::now(),
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
            };
            let binary = meta.serialize().unwrap();
            let meta2 = InternalMetaV1::deserialize(&binary).unwrap();
            assert_eq!(meta.fresh_until, meta2.fresh_until);
        }

        #[test]
        fn test_internal_meta_serde_v2() {
            let meta = InternalMetaV2::default();
            let binary = meta.serialize().unwrap();
            let meta2 = InternalMetaV2::deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);
            assert_eq!(meta.created, meta2.created);
            assert_eq!(meta.updated, meta2.updated);
        }

        #[test]
        fn test_internal_meta_serde_across_versions() {
            let meta = InternalMetaV0 {
                fresh_until: SystemTime::now(),
                created: SystemTime::now(),
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
            };
            let binary = meta.serialize().unwrap();
            let meta2 = deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);

            let meta = InternalMetaV1 {
                version: 1,
                fresh_until: SystemTime::now(),
                created: SystemTime::now(),
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
            };
            let binary = meta.serialize().unwrap();
            let meta2 = deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);
            // `updated` == `created` when upgrading to v2
            assert_eq!(meta2.created, meta2.updated);
        }

        // make sure that v2 format is backward compatible
        // this is the base version of v2 without any extended fields
        #[derive(Deserialize, Serialize)]
        struct InternalMetaV2Base {
            version: u8,
            fresh_until: SystemTime,
            created: SystemTime,
            updated: SystemTime,
            stale_while_revalidate_sec: u32,
            stale_if_error_sec: u32,
        }

        impl InternalMetaV2Base {
            pub const VERSION: u8 = 2;
            pub fn serialize(&self) -> Result<Vec<u8>> {
                assert!(self.version >= Self::VERSION);
                rmp_serde::encode::to_vec(self).or_err(InternalError, "failed to encode cache meta")
            }
            fn deserialize(buf: &[u8]) -> Result<Self> {
                rmp_serde::decode::from_slice(buf)
                    .or_err(InternalError, "failed to decode cache meta v2")
            }
        }

        // this is the base version of v2 with variance but without epoch_override
        #[derive(Deserialize, Serialize)]
        struct InternalMetaV2BaseWithVariance {
            version: u8,
            fresh_until: SystemTime,
            created: SystemTime,
            updated: SystemTime,
            stale_while_revalidate_sec: u32,
            stale_if_error_sec: u32,
            #[serde(default)]
            #[serde(skip_serializing_if = "Option::is_none")]
            variance: Option<HashBinary>,
        }

        impl Default for InternalMetaV2BaseWithVariance {
            fn default() -> Self {
                let epoch = SystemTime::UNIX_EPOCH;
                InternalMetaV2BaseWithVariance {
                    version: InternalMetaV2::VERSION,
                    fresh_until: epoch,
                    created: epoch,
                    updated: epoch,
                    stale_while_revalidate_sec: 0,
                    stale_if_error_sec: 0,
                    variance: None,
                }
            }
        }

        impl InternalMetaV2BaseWithVariance {
            pub const VERSION: u8 = 2;
            pub fn serialize(&self) -> Result<Vec<u8>> {
                assert!(self.version >= Self::VERSION);
                rmp_serde::encode::to_vec(self).or_err(InternalError, "failed to encode cache meta")
            }
            fn deserialize(buf: &[u8]) -> Result<Self> {
                rmp_serde::decode::from_slice(buf)
                    .or_err(InternalError, "failed to decode cache meta v2")
            }
        }

        // V2 with variance + epoch_override fixed in the wire layout, but without
        // provenance. Models the layout produced by reader-prep binaries before
        // provenance writes are enabled.
        #[derive(Deserialize, Serialize)]
        struct InternalMetaV2BeforeProvenance {
            version: u8,
            fresh_until: SystemTime,
            created: SystemTime,
            updated: SystemTime,
            stale_while_revalidate_sec: u32,
            stale_if_error_sec: u32,
            #[serde(default)]
            variance: Option<HashBinary>,
            #[serde(default)]
            epoch_override: Option<SystemTime>,
        }

        impl Default for InternalMetaV2BeforeProvenance {
            fn default() -> Self {
                let epoch = SystemTime::UNIX_EPOCH;
                InternalMetaV2BeforeProvenance {
                    version: InternalMetaV2::VERSION,
                    fresh_until: epoch,
                    created: epoch,
                    updated: epoch,
                    stale_while_revalidate_sec: 0,
                    stale_if_error_sec: 0,
                    variance: None,
                    epoch_override: None,
                }
            }
        }

        impl InternalMetaV2BeforeProvenance {
            pub fn serialize(&self) -> Result<Vec<u8>> {
                rmp_serde::encode::to_vec(self).or_err(InternalError, "failed to encode cache meta")
            }
            fn deserialize(buf: &[u8]) -> Result<Self> {
                rmp_serde::decode::from_slice(buf)
                    .or_err(InternalError, "failed to decode cache meta v2")
            }
        }

        #[test]
        fn test_internal_meta_serde_v2_extend_fields_variance() {
            // ext V2 to base v2
            let meta = InternalMetaV2BaseWithVariance::default();
            let binary = meta.serialize().unwrap();
            let meta2 = InternalMetaV2Base::deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);
            assert_eq!(meta.created, meta2.created);
            assert_eq!(meta.updated, meta2.updated);

            // base V2 to ext v2
            let now = SystemTime::now();
            let meta = InternalMetaV2Base {
                version: InternalMetaV2::VERSION,
                fresh_until: now,
                created: now,
                updated: now,
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
            };
            let binary = meta.serialize().unwrap();
            let meta2 = InternalMetaV2BaseWithVariance::deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);
            assert_eq!(meta.created, meta2.created);
            assert_eq!(meta.updated, meta2.updated);
        }

        #[test]
        fn test_internal_meta_serde_v2_extend_fields_epoch_override() {
            let now = SystemTime::now();

            // Backward compat: pre-epoch_override encodings (V2BaseWithVariance) must
            // still decode into the current InternalMetaV2 with epoch_override = None.
            // This direction is permanent — older on-disk entries written before
            // epoch_override existed must remain readable.
            let mut meta = InternalMetaV2BaseWithVariance {
                version: InternalMetaV2::VERSION,
                fresh_until: now,
                created: now,
                updated: now,
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
                variance: None,
            };
            let binary = meta.serialize().unwrap();
            let meta2 = InternalMetaV2::deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);
            assert_eq!(meta.created, meta2.created);
            assert_eq!(meta.updated, meta2.updated);
            assert!(meta2.variance.is_none());
            assert!(meta2.epoch_override.is_none());

            // Same direction with variance set.
            meta.variance = Some(*b"variance_testing");
            let binary = meta.serialize().unwrap();
            let meta2 = InternalMetaV2::deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);
            assert_eq!(meta.created, meta2.created);
            assert_eq!(meta.updated, meta2.updated);
            assert_eq!(meta.variance, meta2.variance);
            assert!(meta2.epoch_override.is_none());
        }

        // Pins the wire-format change made when removing skip_serializing_if from
        // epoch_override: a Some value and a None value must both round-trip cleanly
        // and produce arrays of the same length. This is the precondition for appending
        // a new optional field after epoch_override in a future release.
        #[test]
        fn test_internal_meta_serde_v2_epoch_override_always_serialized() {
            let now = SystemTime::now();

            let meta_none = InternalMetaV2 {
                fresh_until: now,
                created: now,
                updated: now,
                epoch_override: None,
                ..Default::default()
            };
            let meta_some = InternalMetaV2 {
                fresh_until: now,
                created: now,
                updated: now,
                epoch_override: Some(now),
                ..Default::default()
            };

            let bin_none = meta_none.serialize().unwrap();
            let bin_some = meta_some.serialize().unwrap();

            // Both encodings must produce the same array length so the next appended
            // extended field always lands at the same fixed position regardless of
            // whether epoch_override is set.
            let len_none =
                rmp::decode::read_array_len(&mut &bin_none[..]).expect("decode array len");
            let len_some =
                rmp::decode::read_array_len(&mut &bin_some[..]).expect("decode array len");
            assert_eq!(len_none, len_some);

            // Round-trip both values to confirm decoding still works.
            let decoded_none = InternalMetaV2::deserialize(&bin_none).unwrap();
            let decoded_some = InternalMetaV2::deserialize(&bin_some).unwrap();
            assert!(decoded_none.epoch_override.is_none());
            assert_eq!(decoded_some.epoch_override, Some(now));

            // The same invariant should hold regardless of the preceding variance slot.
            let meta_none_with_variance = InternalMetaV2 {
                fresh_until: now,
                created: now,
                updated: now,
                variance: Some(*b"variance_testing"),
                epoch_override: None,
                ..Default::default()
            };
            let meta_some_with_variance = InternalMetaV2 {
                fresh_until: now,
                created: now,
                updated: now,
                variance: Some(*b"variance_testing"),
                epoch_override: Some(now),
                ..Default::default()
            };
            let bin_none = meta_none_with_variance.serialize().unwrap();
            let bin_some = meta_some_with_variance.serialize().unwrap();
            let len_none =
                rmp::decode::read_array_len(&mut &bin_none[..]).expect("decode array len");
            let len_some =
                rmp::decode::read_array_len(&mut &bin_some[..]).expect("decode array len");
            assert_eq!(len_none, len_some);
        }

        // An on-disk entry written by a pre-provenance binary must decode cleanly
        // into the current schema with provenance = None. The lookup path falls
        // back to `created` for those entries.
        #[test]
        fn test_internal_meta_serde_v2_extend_fields_provenance_backward_compat() {
            let now = SystemTime::now();
            let old = InternalMetaV2BeforeProvenance {
                fresh_until: now,
                created: now,
                updated: now,
                variance: Some(*b"variance_testing"),
                epoch_override: Some(now),
                ..Default::default()
            };
            let binary = old.serialize().unwrap();

            let decoded = InternalMetaV2::deserialize(&binary).unwrap();
            assert_eq!(decoded.version, 2);
            assert_eq!(decoded.fresh_until, now);
            assert_eq!(decoded.created, now);
            assert_eq!(decoded.variance, Some(*b"variance_testing"));
            assert_eq!(decoded.epoch_override, Some(now));
            // The new field is absent from the encoded blob, so serde gives us None.
            assert!(decoded.provenance.is_none());
        }

        // Forward compat: a current encoding with provenance = None must still be
        // decodable by a pre-provenance reader (the field is skipped on the wire when
        // None thanks to skip_serializing_if, keeping the array length equal to the
        // older schema's length).
        #[test]
        fn test_internal_meta_serde_v2_extend_fields_provenance_forward_compat_none() {
            let now = SystemTime::now();
            let current = InternalMetaV2 {
                fresh_until: now,
                created: now,
                updated: now,
                variance: Some(*b"variance_testing"),
                epoch_override: Some(now),
                provenance: None,
                ..Default::default()
            };
            let binary = current.serialize().unwrap();

            // Old reader (no provenance field) accepts this encoding because the
            // array length matches (provenance was skipped during serialization).
            let decoded = InternalMetaV2BeforeProvenance::deserialize(&binary).unwrap();
            assert_eq!(decoded.fresh_until, now);
            assert_eq!(decoded.created, now);
            assert_eq!(decoded.variance, Some(*b"variance_testing"));
            assert_eq!(decoded.epoch_override, Some(now));
        }

        // Entries written with provenance = Some(...) require a reader that supports the
        // provenance field. The previous test pins the compatible None encoding.
        #[test]
        fn test_internal_meta_serde_v2_extend_fields_provenance_some_needs_field_support() {
            let now = SystemTime::now();
            let current = InternalMetaV2 {
                fresh_until: now,
                created: now,
                updated: now,
                variance: Some(*b"variance_testing"),
                epoch_override: Some(now),
                provenance: Some(now),
                ..Default::default()
            };
            let binary = current.serialize().unwrap();

            assert!(InternalMetaV2BeforeProvenance::deserialize(&binary).is_err());
            let decoded = InternalMetaV2::deserialize(&binary).unwrap();
            assert_eq!(decoded.provenance, Some(now));
        }

        // Round-trip a Some(provenance): preservation across encode/decode cycles is
        // what the cache_vary_lookup tombstone relies on for SWR-refreshed entries.
        #[test]
        fn test_internal_meta_serde_v2_provenance_round_trip() {
            let admission = SystemTime::now();
            let updated = admission + Duration::from_secs(300);
            let meta = InternalMetaV2 {
                fresh_until: updated,
                created: updated, // simulates an SWR-refreshed entry: created = now
                updated,
                provenance: Some(admission), // ... but provenance is the ORIGINAL admission
                ..Default::default()
            };
            let binary = meta.serialize().unwrap();
            let decoded = InternalMetaV2::deserialize(&binary).unwrap();
            assert_eq!(decoded.created, updated);
            assert_eq!(decoded.provenance, Some(admission));
        }
    }
}

#[derive(Debug)]
pub(crate) struct CacheMetaInner {
    // http header and Internal meta have different ways of serialization, so keep them separated
    pub(crate) internal: InternalMeta,
    pub(crate) header: ResponseHeader,
    /// An opaque type map to hold extra information for communication between cache backends
    /// and users. This field is **not** guaranteed be persistently stored in the cache backend.
    pub extensions: Extensions,
}

/// The cacheable response header and cache metadata
#[derive(Debug)]
pub struct CacheMeta(pub(crate) Box<CacheMetaInner>);

impl CacheMeta {
    /// Create a [CacheMeta] from the given metadata and the response header
    pub fn new(
        fresh_until: SystemTime,
        created: SystemTime,
        stale_while_revalidate_sec: u32,
        stale_if_error_sec: u32,
        header: ResponseHeader,
    ) -> CacheMeta {
        CacheMeta(Box::new(CacheMetaInner {
            internal: InternalMeta {
                version: InternalMeta::VERSION,
                fresh_until,
                created,
                updated: created, // created == updated for new meta
                stale_while_revalidate_sec,
                stale_if_error_sec,
                provenance: Some(created),
                ..Default::default()
            },
            header,
            extensions: Extensions::new(),
        }))
    }

    /// When the asset was created/admitted to cache
    pub fn created(&self) -> SystemTime {
        self.0.internal.created
    }

    /// The last time the asset was revalidated
    ///
    /// This value will be the same as [Self::created()] if no revalidation ever happens
    pub fn updated(&self) -> SystemTime {
        self.0.internal.updated
    }

    /// Cache-object provenance timestamp.
    ///
    /// When populated, this is a stable reference point for the cache object's
    /// lineage that hit filtering code can use instead of relying on the metadata
    /// record's creation time. The accessor falls back to [`Self::created`] while
    /// the field is absent.
    pub fn provenance(&self) -> SystemTime {
        self.0
            .internal
            .provenance
            .unwrap_or(self.0.internal.created)
    }

    /// Set the cache-object provenance timestamp.
    pub(crate) fn set_provenance(&mut self, provenance: SystemTime) {
        self.0.internal.provenance = Some(provenance);
    }

    /// Reset provenance to this metadata record's creation time.
    pub(crate) fn reset_provenance_to_created(&mut self) {
        self.0.internal.provenance = Some(self.0.internal.created);
    }

    /// The raw provenance value, exposing whether the field was explicitly set
    /// (`Some`) vs derived via the [`Self::created`] fallback (`None`).
    ///
    /// Test-only inspection helper for compatibility coverage.
    #[cfg(test)]
    pub(crate) fn provenance_raw(&self) -> Option<SystemTime> {
        self.0.internal.provenance
    }

    /// The reference point for cache age. This represents the "starting point" for `fresh_until`.
    ///
    /// This defaults to the `updated` timestamp but is overridden by the `epoch_override` field
    /// if set.
    pub fn epoch(&self) -> SystemTime {
        self.0.internal.epoch_override.unwrap_or(self.updated())
    }

    /// Get the epoch override for this asset
    pub fn epoch_override(&self) -> Option<SystemTime> {
        self.0.internal.epoch_override
    }

    /// Set the epoch override for this asset
    ///
    /// When set, this will be used as the reference point for calculating age and freshness
    /// instead of the updated time.
    pub fn set_epoch_override(&mut self, epoch: SystemTime) {
        self.0.internal.epoch_override = Some(epoch);
    }

    /// Remove the epoch override for this asset
    pub fn remove_epoch_override(&mut self) {
        self.0.internal.epoch_override = None;
    }

    /// Is the asset still valid
    pub fn is_fresh(&self, time: SystemTime) -> bool {
        // NOTE: HTTP cache time resolution is second
        self.0.internal.fresh_until >= time
    }

    /// How long (in seconds) the asset should be fresh since its admission/revalidation
    ///
    /// This is essentially the max-age value (or its equivalence).
    /// If an epoch override is set, it will be used as the reference point instead of the updated time.
    pub fn fresh_sec(&self) -> u64 {
        // swallow `duration_since` error, assets that are always stale have earlier `fresh_until` than `created`
        // practically speaking we can always treat these as 0 ttl
        // XXX: return Error if `fresh_until` is much earlier than expected?
        let reference = self.epoch();
        self.0
            .internal
            .fresh_until
            .duration_since(reference)
            .map_or(0, |duration| duration.as_secs())
    }

    /// Until when the asset is considered fresh
    pub fn fresh_until(&self) -> SystemTime {
        self.0.internal.fresh_until
    }

    /// How old the asset is since its admission/revalidation
    ///
    /// If an epoch override is set, it will be used as the reference point instead of the updated time.
    pub fn age(&self) -> Duration {
        let reference = self.epoch();
        SystemTime::now()
            .duration_since(reference)
            .unwrap_or_default()
    }

    /// The stale-while-revalidate limit in seconds
    pub fn stale_while_revalidate_sec(&self) -> u32 {
        self.0.internal.stale_while_revalidate_sec
    }

    /// The stale-if-error limit in seconds
    pub fn stale_if_error_sec(&self) -> u32 {
        self.0.internal.stale_if_error_sec
    }

    /// Can the asset be used to serve stale during revalidation at the given time.
    ///
    /// NOTE: the serve stale functions do not check !is_fresh(time),
    /// i.e. the object is already assumed to be stale.
    pub fn serve_stale_while_revalidate(&self, time: SystemTime) -> bool {
        self.can_serve_stale(self.0.internal.stale_while_revalidate_sec, time)
    }

    /// Can the asset be used to serve stale after error at the given time.
    ///
    /// NOTE: the serve stale functions do not check !is_fresh(time),
    /// i.e. the object is already assumed to be stale.
    pub fn serve_stale_if_error(&self, time: SystemTime) -> bool {
        self.can_serve_stale(self.0.internal.stale_if_error_sec, time)
    }

    /// Disable serve stale for this asset
    pub fn disable_serve_stale(&mut self) {
        self.0.internal.stale_if_error_sec = 0;
        self.0.internal.stale_while_revalidate_sec = 0;
    }

    /// Get the variance hash of this asset
    pub fn variance(&self) -> Option<HashBinary> {
        self.0.internal.variance
    }

    /// Set the variance key of this asset
    pub fn set_variance_key(&mut self, variance_key: HashBinary) {
        self.0.internal.variance = Some(variance_key);
    }

    /// Set the variance (hash) of this asset
    pub fn set_variance(&mut self, variance: HashBinary) {
        self.0.internal.variance = Some(variance)
    }

    /// Removes the variance (hash) of this asset
    pub fn remove_variance(&mut self) {
        self.0.internal.variance = None
    }

    /// Get the response header in this asset
    pub fn response_header(&self) -> &ResponseHeader {
        &self.0.header
    }

    /// Modify the header in this asset
    pub fn response_header_mut(&mut self) -> &mut ResponseHeader {
        &mut self.0.header
    }

    /// Expose the extensions to read
    pub fn extensions(&self) -> &Extensions {
        &self.0.extensions
    }

    /// Expose the extensions to modify
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.0.extensions
    }

    /// Get a copy of the response header
    pub fn response_header_copy(&self) -> ResponseHeader {
        self.0.header.clone()
    }

    /// get all the headers of this asset
    pub fn headers(&self) -> &HMap {
        &self.0.header.headers
    }

    fn can_serve_stale(&self, serve_stale_sec: u32, time: SystemTime) -> bool {
        if serve_stale_sec == 0 {
            return false;
        }
        if let Some(stale_until) = self
            .0
            .internal
            .fresh_until
            .checked_add(Duration::from_secs(serve_stale_sec.into()))
        {
            stale_until >= time
        } else {
            // overflowed: treat as infinite ttl
            true
        }
    }

    /// Serialize this object
    pub fn serialize(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let internal = self.0.internal.serialize()?;
        let header = header_serialize(&self.0.header)?;
        log::debug!("header to serialize: {:?}", &self.0.header);
        Ok((internal, header))
    }

    /// Deserialize from the binary format
    pub fn deserialize(internal: &[u8], header: &[u8]) -> Result<Self> {
        let internal = internal_meta::deserialize(internal)?;
        let header = header_deserialize(header)?;
        Ok(CacheMeta(Box::new(CacheMetaInner {
            internal,
            header,
            extensions: Extensions::new(),
        })))
    }
}

use http::StatusCode;

/// The function to generate TTL from the given [StatusCode].
pub type FreshDurationByStatusFn = fn(StatusCode) -> Option<Duration>;

/// The default settings to generate [CacheMeta]
pub struct CacheMetaDefaults {
    // if a status code is not included in fresh_sec, it's not considered cacheable by default.
    fresh_sec_fn: FreshDurationByStatusFn,
    stale_while_revalidate_sec: u32,
    // TODO: allow "error" condition to be configurable?
    stale_if_error_sec: u32,
}

impl CacheMetaDefaults {
    /// Create a new [CacheMetaDefaults]
    pub const fn new(
        fresh_sec_fn: FreshDurationByStatusFn,
        stale_while_revalidate_sec: u32,
        stale_if_error_sec: u32,
    ) -> Self {
        CacheMetaDefaults {
            fresh_sec_fn,
            stale_while_revalidate_sec,
            stale_if_error_sec,
        }
    }

    /// Return the default TTL for the given [StatusCode]
    ///
    /// `None`: do no cache this code.
    pub fn fresh_sec(&self, resp_status: StatusCode) -> Option<Duration> {
        // safe guard to make sure 304 response to share the same default ttl of 200
        if resp_status == StatusCode::NOT_MODIFIED {
            (self.fresh_sec_fn)(StatusCode::OK)
        } else {
            (self.fresh_sec_fn)(resp_status)
        }
    }

    /// The default SWR seconds
    pub fn serve_stale_while_revalidate_sec(&self) -> u32 {
        self.stale_while_revalidate_sec
    }

    /// The default SIE seconds
    pub fn serve_stale_if_error_sec(&self) -> u32 {
        self.stale_if_error_sec
    }
}

/// The dictionary content for header compression.
///
/// Used during initialization of [`HEADER_SERDE`].
static COMPRESSION_DICT_CONTENT: OnceCell<Cow<'static, [u8]>> = OnceCell::new();

static HEADER_SERDE: Lazy<HeaderSerde> = Lazy::new(|| {
    let dict_opt = if let Some(dict_content) = COMPRESSION_DICT_CONTENT.get() {
        Some(dict_content.to_vec())
    } else {
        warn!("no header compression dictionary loaded - use set_compression_dict_content() or set_compression_dict_path() to set one");
        None
    };

    HeaderSerde::new(dict_opt)
});

pub(crate) fn header_serialize(header: &ResponseHeader) -> Result<Vec<u8>> {
    HEADER_SERDE.serialize(header)
}

pub(crate) fn header_deserialize<T: AsRef<[u8]>>(buf: T) -> Result<ResponseHeader> {
    HEADER_SERDE.deserialize(buf.as_ref())
}

/// Load the header compression dictionary from a file, which helps serialize http header.
///
/// Returns false if it is already set or if the file could not be read.
///
/// Use [`set_compression_dict_content`] to set the dictionary from memory instead.
pub fn set_compression_dict_path(path: &str) -> bool {
    match std::fs::read(path) {
        Ok(dict) => COMPRESSION_DICT_CONTENT.set(dict.into()).is_ok(),
        Err(e) => {
            warn!(
                "failed to read header compress dictionary file at {}, {:?}",
                path, e
            );
            false
        }
    }
}

/// Set the header compression dictionary content, which helps serialize http header.
///
/// Returns false if it is already set.
///
/// This is an alernative to [`set_compression_dict_path`], allowing use of
/// a dictionary without an external file.
pub fn set_compression_dict_content(content: Cow<'static, [u8]>) -> bool {
    COMPRESSION_DICT_CONTENT.set(content).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_cache_meta_age_without_override() {
        let now = SystemTime::now();
        let header = ResponseHeader::build_no_case(200, None).unwrap();
        let meta = CacheMeta::new(now + Duration::from_secs(300), now, 0, 0, header);

        // Without epoch_override, age() should use updated() as reference
        std::thread::sleep(Duration::from_millis(100));
        let age = meta.age();
        assert!(age.as_secs() < 1, "age should be close to 0");

        // epoch() should return updated() when no override is set
        assert_eq!(meta.epoch(), meta.updated());
    }

    #[test]
    fn test_cache_meta_age_with_epoch_override_past() {
        let now = SystemTime::now();
        let header = ResponseHeader::build(200, None).unwrap();
        let mut meta = CacheMeta::new(now + Duration::from_secs(300), now, 0, 0, header);

        // Set epoch_override to 10 seconds in the past
        let epoch_override = now - Duration::from_secs(10);
        meta.set_epoch_override(epoch_override);

        // age() should now use epoch_override as the reference
        let age = meta.age();
        assert!(age.as_secs() >= 10);
        assert!(age.as_secs() < 12);

        // epoch() should return the override
        assert_eq!(meta.epoch(), epoch_override);
        assert_eq!(meta.epoch_override(), Some(epoch_override));
    }

    #[test]
    fn test_cache_meta_age_with_epoch_override_future() {
        let now = SystemTime::now();
        let header = ResponseHeader::build(200, None).unwrap();
        let mut meta = CacheMeta::new(now + Duration::from_secs(100), now, 0, 0, header);

        // Set epoch_override to a future time
        let future_epoch = now + Duration::from_secs(10);
        meta.set_epoch_override(future_epoch);

        let age_with_epoch = meta.age();
        // age should be 0 since epoch_override is in the future
        assert_eq!(age_with_epoch, Duration::ZERO);
    }

    #[test]
    fn test_cache_meta_fresh_sec() {
        let header = ResponseHeader::build(StatusCode::OK, None).unwrap();
        let mut meta = CacheMeta::new(
            SystemTime::now() + Duration::from_secs(100),
            SystemTime::now() - Duration::from_secs(100),
            0,
            0,
            header,
        );

        meta.0.internal.updated = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        meta.0.internal.fresh_until = SystemTime::UNIX_EPOCH + Duration::from_secs(1100);

        // Without epoch_override, fresh_sec should use updated as reference
        let fresh_sec_without_override = meta.fresh_sec();
        assert_eq!(fresh_sec_without_override, 100); // 1100 - 1000 = 100 seconds

        // With epoch_override set to a later time (1050), fresh_sec should be calculated from that reference
        let epoch_override = SystemTime::UNIX_EPOCH + Duration::from_secs(1050);
        meta.set_epoch_override(epoch_override);
        assert_eq!(meta.epoch_override(), Some(epoch_override));
        assert_eq!(meta.epoch(), epoch_override);

        let fresh_sec_with_override = meta.fresh_sec();
        // fresh_until - epoch_override = 1100 - 1050 = 50 seconds
        assert_eq!(fresh_sec_with_override, 50);

        meta.remove_epoch_override();
        assert_eq!(meta.epoch_override(), None);
        assert_eq!(meta.epoch(), meta.updated());
        assert_eq!(meta.fresh_sec(), 100); // back to normal calculation
    }

    #[test]
    fn test_cache_meta_new_stamps_provenance() {
        let now = SystemTime::now();
        let header = ResponseHeader::build(StatusCode::OK, None).unwrap();
        let meta = CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header);

        assert_eq!(meta.created(), now);
        assert_eq!(meta.provenance(), now);
        assert_eq!(meta.provenance_raw(), Some(now));
    }

    #[test]
    fn test_cache_meta_provenance_fallback_for_absent_field() {
        let admission = SystemTime::now();
        let header = ResponseHeader::build(StatusCode::OK, None).unwrap();
        let mut meta = CacheMeta::new(admission + Duration::from_secs(60), admission, 0, 0, header);
        meta.0.internal.provenance = None;

        assert_eq!(meta.created(), admission);
        assert!(meta.provenance_raw().is_none());
        // Fallback path: provenance() returns created().
        assert_eq!(meta.provenance(), admission);
    }

    #[test]
    fn test_cache_meta_set_provenance() {
        let admission = SystemTime::now();
        let provenance = admission - Duration::from_secs(30);
        let header = ResponseHeader::build(StatusCode::OK, None).unwrap();
        let mut meta = CacheMeta::new(admission + Duration::from_secs(60), admission, 0, 0, header);

        meta.set_provenance(provenance);

        assert_eq!(meta.created(), admission);
        assert_eq!(meta.provenance(), provenance);
        assert_eq!(meta.provenance_raw(), Some(provenance));
    }
}
