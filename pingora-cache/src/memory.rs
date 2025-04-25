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

//! Hash map based in memory cache
//!
//! For testing only, not for production use

//TODO: Mark this module #[test] only

use super::*;
use crate::key::CompactCacheKey;
use crate::storage::{streaming_write::U64WriteId, HandleHit, HandleMiss};
use crate::trace::SpanHandle;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use pingora_error::*;
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

type BinaryMeta = (Vec<u8>, Vec<u8>);

pub(crate) struct CacheObject {
    pub meta: BinaryMeta,
    pub body: Arc<Vec<u8>>,
}

pub(crate) struct TempObject {
    pub meta: BinaryMeta,
    // these are Arc because they need to continue to exist after this TempObject is removed
    pub body: Arc<RwLock<Vec<u8>>>,
    bytes_written: Arc<watch::Sender<PartialState>>, // this should match body.len()
}

impl TempObject {
    fn new(meta: BinaryMeta) -> Self {
        let (tx, _rx) = watch::channel(PartialState::Partial(0));
        TempObject {
            meta,
            body: Arc::new(RwLock::new(Vec::new())),
            bytes_written: Arc::new(tx),
        }
    }
    // this is not at all optimized
    fn make_cache_object(&self) -> CacheObject {
        let meta = self.meta.clone();
        let body = Arc::new(self.body.read().clone());
        CacheObject { meta, body }
    }
}

/// Hash map based in memory cache
///
/// For testing only, not for production use.
pub struct MemCache {
    pub(crate) cached: Arc<RwLock<HashMap<String, CacheObject>>>,
    pub(crate) temp: Arc<RwLock<HashMap<String, HashMap<u64, TempObject>>>>,
    pub(crate) last_temp_id: AtomicU64,
}

impl MemCache {
    /// Create a new [MemCache]
    pub fn new() -> Self {
        MemCache {
            cached: Arc::new(RwLock::new(HashMap::new())),
            temp: Arc::new(RwLock::new(HashMap::new())),
            last_temp_id: AtomicU64::new(0),
        }
    }
}

pub enum MemHitHandler {
    Complete(CompleteHit),
    Partial(PartialHit),
}

#[derive(Copy, Clone)]
enum PartialState {
    Partial(usize),
    Complete(usize),
}

pub struct CompleteHit {
    body: Arc<Vec<u8>>,
    done: bool,
    range_start: usize,
    range_end: usize,
}

impl CompleteHit {
    fn get(&mut self) -> Option<Bytes> {
        if self.done {
            None
        } else {
            self.done = true;
            Some(Bytes::copy_from_slice(
                &self.body.as_slice()[self.range_start..self.range_end],
            ))
        }
    }

    fn seek(&mut self, start: usize, end: Option<usize>) -> Result<()> {
        if start >= self.body.len() {
            return Error::e_explain(
                ErrorType::InternalError,
                format!("seek start out of range {start} >= {}", self.body.len()),
            );
        }
        self.range_start = start;
        if let Some(end) = end {
            // end over the actual last byte is allowed, we just need to return the actual bytes
            self.range_end = std::cmp::min(self.body.len(), end);
        }
        // seek resets read so that one handler can be used for multiple ranges
        self.done = false;
        Ok(())
    }
}

pub struct PartialHit {
    body: Arc<RwLock<Vec<u8>>>,
    bytes_written: watch::Receiver<PartialState>,
    bytes_read: usize,
}

impl PartialHit {
    async fn read(&mut self) -> Option<Bytes> {
        loop {
            let bytes_written = *self.bytes_written.borrow_and_update();
            let bytes_end = match bytes_written {
                PartialState::Partial(s) => s,
                PartialState::Complete(c) => {
                    // no more data will arrive
                    if c == self.bytes_read {
                        return None;
                    }
                    c
                }
            };
            assert!(bytes_end >= self.bytes_read);

            // more data available to read
            if bytes_end > self.bytes_read {
                let new_bytes =
                    Bytes::copy_from_slice(&self.body.read()[self.bytes_read..bytes_end]);
                self.bytes_read = bytes_end;
                return Some(new_bytes);
            }

            // wait for more data
            if self.bytes_written.changed().await.is_err() {
                // err: sender dropped, body is finished
                // FIXME: sender could drop because of an error
                return None;
            }
        }
    }
}

#[async_trait]
impl HandleHit for MemHitHandler {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        match self {
            Self::Complete(c) => Ok(c.get()),
            Self::Partial(p) => Ok(p.read().await),
        }
    }
    async fn finish(
        self: Box<Self>, // because self is always used as a trait object
        _storage: &'static (dyn storage::Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool {
        match self {
            Self::Complete(_) => true,
            Self::Partial(_) => false, // TODO: support seeking in partial reads
        }
    }

    fn seek(&mut self, start: usize, end: Option<usize>) -> Result<()> {
        match self {
            Self::Complete(c) => c.seek(start, end),
            Self::Partial(_) => Error::e_explain(
                ErrorType::InternalError,
                "seek not supported for partial cache",
            ),
        }
    }

    fn should_count_access(&self) -> bool {
        match self {
            // avoid counting accesses for partial reads to keep things simple
            Self::Complete(_) => true,
            Self::Partial(_) => false,
        }
    }

    fn get_eviction_weight(&self) -> usize {
        match self {
            // FIXME: just body size, also track meta size
            Self::Complete(c) => c.body.len(),
            // partial read cannot be estimated since body size is unknown
            Self::Partial(_) => 0,
        }
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

pub struct MemMissHandler {
    body: Arc<RwLock<Vec<u8>>>,
    bytes_written: Arc<watch::Sender<PartialState>>,
    // these are used only in finish() to data from temp to cache
    key: String,
    temp_id: U64WriteId,
    // key -> cache object
    cache: Arc<RwLock<HashMap<String, CacheObject>>>,
    // key -> (temp writer id -> temp object) to support concurrent writers
    temp: Arc<RwLock<HashMap<String, HashMap<u64, TempObject>>>>,
}

#[async_trait]
impl HandleMiss for MemMissHandler {
    async fn write_body(&mut self, data: bytes::Bytes, eof: bool) -> Result<()> {
        let current_bytes = match *self.bytes_written.borrow() {
            PartialState::Partial(p) => p,
            PartialState::Complete(_) => panic!("already EOF"),
        };
        self.body.write().extend_from_slice(&data);
        let written = current_bytes + data.len();
        let new_state = if eof {
            PartialState::Complete(written)
        } else {
            PartialState::Partial(written)
        };
        self.bytes_written.send_replace(new_state);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        // safe, the temp object is inserted when the miss handler is created
        let cache_object = self
            .temp
            .read()
            .get(&self.key)
            .unwrap()
            .get(&self.temp_id.into())
            .unwrap()
            .make_cache_object();
        let size = cache_object.body.len(); // FIXME: this just body size, also track meta size
        self.cache.write().insert(self.key.clone(), cache_object);
        self.temp
            .write()
            .get_mut(&self.key)
            .and_then(|map| map.remove(&self.temp_id.into()));
        Ok(MissFinishType::Created(size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(self.temp_id.as_bytes())
    }
}

impl Drop for MemMissHandler {
    fn drop(&mut self) {
        self.temp
            .write()
            .get_mut(&self.key)
            .and_then(|map| map.remove(&self.temp_id.into()));
    }
}

fn hit_from_temp_obj(temp_obj: &TempObject) -> Result<Option<(CacheMeta, HitHandler)>> {
    let meta = CacheMeta::deserialize(&temp_obj.meta.0, &temp_obj.meta.1)?;
    let partial = PartialHit {
        body: temp_obj.body.clone(),
        bytes_written: temp_obj.bytes_written.subscribe(),
        bytes_read: 0,
    };
    let hit_handler = MemHitHandler::Partial(partial);
    Ok(Some((meta, Box::new(hit_handler))))
}

#[async_trait]
impl Storage for MemCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let hash = key.combined();
        // always prefer partial read otherwise fresh asset will not be visible on expired asset
        // until it is fully updated
        // no preference on which partial read we get (if there are multiple writers)
        if let Some((_, temp_obj)) = self
            .temp
            .read()
            .get(&hash)
            .and_then(|map| map.iter().next())
        {
            hit_from_temp_obj(temp_obj)
        } else if let Some(obj) = self.cached.read().get(&hash) {
            let meta = CacheMeta::deserialize(&obj.meta.0, &obj.meta.1)?;
            let hit_handler = CompleteHit {
                body: obj.body.clone(),
                done: false,
                range_start: 0,
                range_end: obj.body.len(),
            };
            let hit_handler = MemHitHandler::Complete(hit_handler);
            Ok(Some((meta, Box::new(hit_handler))))
        } else {
            Ok(None)
        }
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        _trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let hash = key.combined();
        let write_tag: U64WriteId = streaming_write_tag
            .expect("tag must be set during streaming write")
            .try_into()
            .expect("tag must be correct length");
        hit_from_temp_obj(
            self.temp
                .read()
                .get(&hash)
                .and_then(|map| map.get(&write_tag.into()))
                .expect("must have partial write in progress"),
        )
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<MissHandler> {
        let hash = key.combined();
        let meta = meta.serialize()?;
        let temp_obj = TempObject::new(meta);
        let temp_id = self.last_temp_id.fetch_add(1, Ordering::Relaxed);
        let miss_handler = MemMissHandler {
            body: temp_obj.body.clone(),
            bytes_written: temp_obj.bytes_written.clone(),
            key: hash.clone(),
            cache: self.cached.clone(),
            temp: self.temp.clone(),
            temp_id: temp_id.into(),
        };
        self.temp
            .write()
            .entry(hash)
            .or_default()
            .insert(miss_handler.temp_id.into(), temp_obj);
        Ok(Box::new(miss_handler))
    }

    async fn purge(
        &'static self,
        key: &CompactCacheKey,
        _type: PurgeType,
        _trace: &SpanHandle,
    ) -> Result<bool> {
        // This usually purges the primary key because, without a lookup, the variance key is usually
        // empty
        let hash = key.combined();
        let temp_removed = self.temp.write().remove(&hash).is_some();
        let cache_removed = self.cached.write().remove(&hash).is_some();
        Ok(temp_removed || cache_removed)
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<bool> {
        let hash = key.combined();
        if let Some(obj) = self.cached.write().get_mut(&hash) {
            obj.meta = meta.serialize()?;
            Ok(true)
        } else {
            panic!("no meta found")
        }
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use cf_rustracing::span::Span;
    use once_cell::sync::Lazy;

    fn gen_meta() -> CacheMeta {
        let mut header = ResponseHeader::build(200, None).unwrap();
        header.append_header("foo1", "bar1").unwrap();
        header.append_header("foo2", "bar2").unwrap();
        header.append_header("foo3", "bar3").unwrap();
        header.append_header("Server", "Pingora").unwrap();
        let internal = crate::meta::InternalMeta::default();
        CacheMeta(Box::new(crate::meta::CacheMetaInner {
            internal,
            header,
            extensions: http::Extensions::new(),
        }))
    }

    #[tokio::test]
    async fn test_write_then_read() {
        static MEM_CACHE: Lazy<MemCache> = Lazy::new(MemCache::new);
        let span = &Span::inactive().handle();

        let key1 = CacheKey::new("", "a", "1");
        let res = MEM_CACHE.lookup(&key1, span).await.unwrap();
        assert!(res.is_none());

        let cache_meta = gen_meta();

        let mut miss_handler = MEM_CACHE
            .get_miss_handler(&key1, &cache_meta, span)
            .await
            .unwrap();
        miss_handler
            .write_body(b"test1"[..].into(), false)
            .await
            .unwrap();
        miss_handler
            .write_body(b"test2"[..].into(), false)
            .await
            .unwrap();
        miss_handler.finish().await.unwrap();

        let (cache_meta2, mut hit_handler) = MEM_CACHE.lookup(&key1, span).await.unwrap().unwrap();
        assert_eq!(
            cache_meta.0.internal.fresh_until,
            cache_meta2.0.internal.fresh_until
        );

        let data = hit_handler.read_body().await.unwrap().unwrap();
        assert_eq!("test1test2", data);
        let data = hit_handler.read_body().await.unwrap();
        assert!(data.is_none());
    }

    #[tokio::test]
    async fn test_read_range() {
        static MEM_CACHE: Lazy<MemCache> = Lazy::new(MemCache::new);
        let span = &Span::inactive().handle();

        let key1 = CacheKey::new("", "a", "1");
        let res = MEM_CACHE.lookup(&key1, span).await.unwrap();
        assert!(res.is_none());

        let cache_meta = gen_meta();

        let mut miss_handler = MEM_CACHE
            .get_miss_handler(&key1, &cache_meta, span)
            .await
            .unwrap();
        miss_handler
            .write_body(b"test1test2"[..].into(), false)
            .await
            .unwrap();
        miss_handler.finish().await.unwrap();

        let (cache_meta2, mut hit_handler) = MEM_CACHE.lookup(&key1, span).await.unwrap().unwrap();
        assert_eq!(
            cache_meta.0.internal.fresh_until,
            cache_meta2.0.internal.fresh_until
        );

        // out of range
        assert!(hit_handler.seek(10000, None).is_err());

        assert!(hit_handler.seek(5, None).is_ok());
        let data = hit_handler.read_body().await.unwrap().unwrap();
        assert_eq!("test2", data);
        let data = hit_handler.read_body().await.unwrap();
        assert!(data.is_none());

        assert!(hit_handler.seek(4, Some(5)).is_ok());
        let data = hit_handler.read_body().await.unwrap().unwrap();
        assert_eq!("1", data);
        let data = hit_handler.read_body().await.unwrap();
        assert!(data.is_none());
    }

    #[tokio::test]
    async fn test_write_while_read() {
        use futures::FutureExt;

        static MEM_CACHE: Lazy<MemCache> = Lazy::new(MemCache::new);
        let span = &Span::inactive().handle();

        let key1 = CacheKey::new("", "a", "1");
        let res = MEM_CACHE.lookup(&key1, span).await.unwrap();
        assert!(res.is_none());

        let cache_meta = gen_meta();

        let mut miss_handler = MEM_CACHE
            .get_miss_handler(&key1, &cache_meta, span)
            .await
            .unwrap();

        // first reader
        let (cache_meta1, mut hit_handler1) = MEM_CACHE.lookup(&key1, span).await.unwrap().unwrap();
        assert_eq!(
            cache_meta.0.internal.fresh_until,
            cache_meta1.0.internal.fresh_until
        );

        // No body to read
        let res = hit_handler1.read_body().now_or_never();
        assert!(res.is_none());

        miss_handler
            .write_body(b"test1"[..].into(), false)
            .await
            .unwrap();

        let data = hit_handler1.read_body().await.unwrap().unwrap();
        assert_eq!("test1", data);
        let res = hit_handler1.read_body().now_or_never();
        assert!(res.is_none());

        miss_handler
            .write_body(b"test2"[..].into(), false)
            .await
            .unwrap();
        let data = hit_handler1.read_body().await.unwrap().unwrap();
        assert_eq!("test2", data);

        // second reader
        let (cache_meta2, mut hit_handler2) = MEM_CACHE.lookup(&key1, span).await.unwrap().unwrap();
        assert_eq!(
            cache_meta.0.internal.fresh_until,
            cache_meta2.0.internal.fresh_until
        );

        let data = hit_handler2.read_body().await.unwrap().unwrap();
        assert_eq!("test1test2", data);
        let res = hit_handler2.read_body().now_or_never();
        assert!(res.is_none());

        let res = hit_handler1.read_body().now_or_never();
        assert!(res.is_none());

        miss_handler.finish().await.unwrap();

        let data = hit_handler1.read_body().await.unwrap();
        assert!(data.is_none());
        let data = hit_handler2.read_body().await.unwrap();
        assert!(data.is_none());
    }

    #[tokio::test]
    async fn test_purge_partial() {
        static MEM_CACHE: Lazy<MemCache> = Lazy::new(MemCache::new);
        let cache = &MEM_CACHE;

        let key = CacheKey::new("", "a", "1").to_compact();
        let hash = key.combined();
        let meta = (
            "meta_key".as_bytes().to_vec(),
            "meta_value".as_bytes().to_vec(),
        );

        let temp_obj = TempObject::new(meta);
        let mut map = HashMap::new();
        map.insert(0, temp_obj);
        cache.temp.write().insert(hash.clone(), map);

        assert!(cache.temp.read().contains_key(&hash));

        let result = cache
            .purge(&key, PurgeType::Invalidation, &Span::inactive().handle())
            .await;
        assert!(result.is_ok());

        assert!(!cache.temp.read().contains_key(&hash));
    }

    #[tokio::test]
    async fn test_purge_complete() {
        static MEM_CACHE: Lazy<MemCache> = Lazy::new(MemCache::new);
        let cache = &MEM_CACHE;

        let key = CacheKey::new("", "a", "1").to_compact();
        let hash = key.combined();
        let meta = (
            "meta_key".as_bytes().to_vec(),
            "meta_value".as_bytes().to_vec(),
        );
        let body = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 0];
        let cache_obj = CacheObject {
            meta,
            body: Arc::new(body),
        };
        cache.cached.write().insert(hash.clone(), cache_obj);

        assert!(cache.cached.read().contains_key(&hash));

        let result = cache
            .purge(&key, PurgeType::Invalidation, &Span::inactive().handle())
            .await;
        assert!(result.is_ok());

        assert!(!cache.cached.read().contains_key(&hash));
    }
}
