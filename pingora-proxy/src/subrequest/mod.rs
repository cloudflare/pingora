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

use async_trait::async_trait;
use core::pin::Pin;
use core::task::{Context, Poll};
use pingora_cache::lock::{CacheKeyLockImpl, LockStatus, WritePermit};
use pingora_cache::CacheKey;
use pingora_core::protocols::raw_connect::ProxyDigest;
use pingora_core::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, SocketDigest, Ssl, TimingDigest,
    UniqueID, UniqueIDType,
};
use std::any::Any;
use std::io::Cursor;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, Error, ReadBuf};

// An async IO stream that returns the request when being read from and dumps the data to the void
// when being write to
#[derive(Debug)]
pub(crate) struct DummyIO(Cursor<Vec<u8>>);

impl DummyIO {
    pub fn new(read_bytes: &[u8]) -> Self {
        DummyIO(Cursor::new(Vec::from(read_bytes)))
    }
}

impl AsyncRead for DummyIO {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), Error>> {
        if self.0.position() < self.0.get_ref().len() as u64 {
            Pin::new(&mut self.0).poll_read(cx, buf)
        } else {
            // all data is read, pending forever otherwise the stream is considered closed
            Poll::Pending
        }
    }
}

impl AsyncWrite for DummyIO {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
}

impl UniqueID for DummyIO {
    fn id(&self) -> UniqueIDType {
        0 // placeholder
    }
}

impl Ssl for DummyIO {}

impl GetTimingDigest for DummyIO {
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        vec![]
    }
}

impl GetProxyDigest for DummyIO {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        None
    }
}

impl GetSocketDigest for DummyIO {
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        None
    }
}

impl Peek for DummyIO {}

#[async_trait]
impl pingora_core::protocols::Shutdown for DummyIO {
    async fn shutdown(&mut self) -> () {}
}

#[tokio::test]
async fn test_dummy_io() {
    use futures::FutureExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut dummy = DummyIO::new(&[1, 2]);
    let res = dummy.read_u8().await;
    assert_eq!(res.unwrap(), 1);
    let res = dummy.read_u8().await;
    assert_eq!(res.unwrap(), 2);
    let res = dummy.read_u8().now_or_never();
    assert!(res.is_none()); // pending forever
    let res = dummy.write_u8(0).await;
    assert!(res.is_ok());
}

struct LockCtx {
    write_permit: WritePermit,
    cache_lock: &'static CacheKeyLockImpl,
    key: CacheKey,
}

/// Optional user-defined subrequest context.
pub type UserCtx = Box<(dyn Any + Sync + Send)>;

#[derive(Default)]
pub struct CtxBuilder {
    lock: Option<LockCtx>,
    user_ctx: Option<UserCtx>,
}

impl CtxBuilder {
    pub fn new() -> Self {
        Self {
            lock: None,
            user_ctx: None,
        }
    }

    pub fn cache_write_lock(
        mut self,
        cache_lock: &'static CacheKeyLockImpl,
        key: CacheKey,
        write_permit: WritePermit,
    ) -> Self {
        self.lock = Some(LockCtx {
            cache_lock,
            key,
            write_permit,
        });
        self
    }

    pub fn user_ctx(mut self, user_ctx: UserCtx) -> Self {
        self.user_ctx = Some(user_ctx);
        self
    }

    pub fn build(self) -> Ctx {
        Ctx {
            lock: self.lock,
            user_ctx: self.user_ctx,
        }
    }
}

/// Context struct to share state across the parent and sub-request.
pub struct Ctx {
    lock: Option<LockCtx>,
    // User-defined custom context.
    user_ctx: Option<UserCtx>,
}

impl Ctx {
    /// Create a [`CtxBuilder`] in order to make a new subrequest `Ctx`.
    pub fn builder() -> CtxBuilder {
        CtxBuilder::new()
    }

    /// Get a reference to the extensions inside this subrequest.
    pub fn user_ctx(&self) -> Option<&UserCtx> {
        self.user_ctx.as_ref()
    }

    /// Get a mutable reference to the extensions inside this subrequest.
    pub fn user_ctx_mut(&mut self) -> Option<&mut UserCtx> {
        self.user_ctx.as_mut()
    }

    /// Release the write lock from the subrequest (to clean up a write permit
    /// that will not be used in the cache key lock).
    pub fn release_write_lock(&mut self) {
        if let Some(lock) = self.lock.take() {
            // If we are releasing the write lock in the subrequest,
            // it means that the cache did not take it for whatever reason.
            // TransientError will cause the election of a new writer
            lock.cache_lock
                .release(&lock.key, lock.write_permit, LockStatus::TransientError);
        }
    }

    /// Take the write lock from the subrequest, for use in a cache key lock.
    pub fn take_write_lock(&mut self) -> Option<WritePermit> {
        // also clear out lock ctx
        self.lock.take().map(|lock| lock.write_permit)
    }
}

use crate::HttpSession;

pub(crate) fn create_dummy_session(parsed_session: &HttpSession) -> HttpSession {
    // TODO: check if there is req body, we don't capture the body for now
    HttpSession::new_http1(Box::new(DummyIO::new(&parsed_session.to_h1_raw())))
}

#[tokio::test]
async fn test_dummy_request() {
    use tokio_test::io::Builder;

    let input = b"GET / HTTP/1.1\r\n\r\n";
    let mock_io = Builder::new().read(&input[..]).build();
    let mut req = HttpSession::new_http1(Box::new(mock_io));
    req.read_request().await.unwrap();
    assert_eq!(input.as_slice(), req.to_h1_raw());

    let mut dummy_req = create_dummy_session(&req);
    dummy_req.read_request().await.unwrap();
    assert_eq!(input.as_slice(), req.to_h1_raw());
}
