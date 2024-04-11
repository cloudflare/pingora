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

//! Metadata for caching

use http::Extensions;
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use pingora_http::{HMap, ResponseHeader};
use serde::{Deserialize, Serialize};
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
        #[serde(skip_serializing_if = "Option::is_none")]
        pub(crate) variance: Option<HashBinary>,
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

        #[test]
        fn test_internal_meta_serde_v2_extend_fields() {
            // make sure that v2 format is backward compatible
            // this is the base version of v2 without any extended fields
            #[derive(Deserialize, Serialize)]
            pub(crate) struct InternalMetaV2Base {
                pub(crate) version: u8,
                pub(crate) fresh_until: SystemTime,
                pub(crate) created: SystemTime,
                pub(crate) updated: SystemTime,
                pub(crate) stale_while_revalidate_sec: u32,
                pub(crate) stale_if_error_sec: u32,
            }

            impl InternalMetaV2Base {
                pub const VERSION: u8 = 2;
                pub fn serialize(&self) -> Result<Vec<u8>> {
                    assert!(self.version >= Self::VERSION);
                    rmp_serde::encode::to_vec(self)
                        .or_err(InternalError, "failed to encode cache meta")
                }
                fn deserialize(buf: &[u8]) -> Result<Self> {
                    rmp_serde::decode::from_slice(buf)
                        .or_err(InternalError, "failed to decode cache meta v2")
                }
            }

            // ext V2 to base v2
            let meta = InternalMetaV2::default();
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
            let meta2 = InternalMetaV2::deserialize(&binary).unwrap();
            assert_eq!(meta2.version, 2);
            assert_eq!(meta.fresh_until, meta2.fresh_until);
            assert_eq!(meta.created, meta2.created);
            assert_eq!(meta.updated, meta2.updated);
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

    /// Is the asset still valid
    pub fn is_fresh(&self, time: SystemTime) -> bool {
        // NOTE: HTTP cache time resolution is second
        self.0.internal.fresh_until >= time
    }

    /// How long (in seconds) the asset should be fresh since its admission/revalidation
    ///
    /// This is essentially the max-age value (or its equivalence)
    pub fn fresh_sec(&self) -> u64 {
        // swallow `duration_since` error, assets that are always stale have earlier `fresh_until` than `created`
        // practically speaking we can always treat these as 0 ttl
        // XXX: return Error if `fresh_until` is much earlier than expected?
        self.0
            .internal
            .fresh_until
            .duration_since(self.0.internal.updated)
            .map_or(0, |duration| duration.as_secs())
    }

    /// Until when the asset is considered fresh
    pub fn fresh_until(&self) -> SystemTime {
        self.0.internal.fresh_until
    }

    /// How old the asset is since its admission/revalidation
    pub fn age(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.updated())
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
pub type FreshSecByStatusFn = fn(StatusCode) -> Option<u32>;

/// The default settings to generate [CacheMeta]
pub struct CacheMetaDefaults {
    // if a status code is not included in fresh_sec, it's not considered cacheable by default.
    fresh_sec_fn: FreshSecByStatusFn,
    stale_while_revalidate_sec: u32,
    // TODO: allow "error" condition to be configurable?
    stale_if_error_sec: u32,
}

impl CacheMetaDefaults {
    /// Create a new [CacheMetaDefaults]
    pub const fn new(
        fresh_sec_fn: FreshSecByStatusFn,
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
    pub fn fresh_sec(&self, resp_status: StatusCode) -> Option<u32> {
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

use log::warn;
use once_cell::sync::{Lazy, OnceCell};
use pingora_header_serde::HeaderSerde;
use std::fs::File;
use std::io::Read;

/* load header compression engine and its dictionary globally */
pub(crate) static COMPRESSION_DICT_PATH: OnceCell<String> = OnceCell::new();

fn load_file(path: &String) -> Option<Vec<u8>> {
    let mut file = File::open(path)
        .map_err(|e| {
            warn!(
                "failed to open header compress dictionary file at {}, {:?}",
                path, e
            );
            e
        })
        .ok()?;
    let mut dict = Vec::new();
    file.read_to_end(&mut dict)
        .map_err(|e| {
            warn!(
                "failed to read header compress dictionary file at {}, {:?}",
                path, e
            );
            e
        })
        .ok()?;

    Some(dict)
}

static HEADER_SERDE: Lazy<HeaderSerde> = Lazy::new(|| {
    let dict = COMPRESSION_DICT_PATH.get().and_then(load_file);
    HeaderSerde::new(dict)
});

pub(crate) fn header_serialize(header: &ResponseHeader) -> Result<Vec<u8>> {
    HEADER_SERDE.serialize(header)
}

pub(crate) fn header_deserialize<T: AsRef<[u8]>>(buf: T) -> Result<ResponseHeader> {
    HEADER_SERDE.deserialize(buf.as_ref())
}
