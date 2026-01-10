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

use pingora_cache::lock::{CacheKeyLockImpl, LockStatus, WritePermit};
use pingora_cache::CacheKey;
use pingora_core::protocols::http::subrequest::server::{
    HttpSession as SessionSubrequest, SubrequestHandle,
};
use std::any::Any;

struct LockCtx {
    write_permit: WritePermit,
    cache_lock: &'static CacheKeyLockImpl,
    key: CacheKey,
}

/// Optional user-defined subrequest context.
pub type UserCtx = Box<dyn Any + Sync + Send>;

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub enum BodyMode {
    /// No body to be sent for subrequest.
    #[default]
    NoBody,
    /// Waiting on body if needed.
    ExpectBody,
}

#[derive(Default)]
pub struct CtxBuilder {
    lock: Option<LockCtx>,
    body_mode: BodyMode,
    user_ctx: Option<UserCtx>,
}

impl CtxBuilder {
    pub fn new() -> Self {
        Self {
            lock: None,
            body_mode: BodyMode::NoBody,
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

    pub fn body_mode(mut self, body_mode: BodyMode) -> Self {
        self.body_mode = body_mode;
        self
    }

    pub fn build(self) -> Ctx {
        Ctx {
            lock: self.lock,
            body_mode: self.body_mode,
            user_ctx: self.user_ctx,
        }
    }
}

/// Context struct to share state across the parent and sub-request.
pub struct Ctx {
    body_mode: BodyMode,
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

    /// Get the `BodyMode` when this subrequest was created.
    pub fn body_mode(&self) -> BodyMode {
        self.body_mode
    }
}

use crate::HttpSession;

pub(crate) fn create_session(parsed_session: &HttpSession) -> (HttpSession, SubrequestHandle) {
    let (session, handle) = SessionSubrequest::new_from_session(parsed_session);
    (HttpSession::new_subrequest(session), handle)
}

#[tokio::test]
async fn test_dummy_request() {
    use tokio_test::io::Builder;

    let input = b"GET / HTTP/1.1\r\n\r\n";
    let mock_io = Builder::new().read(&input[..]).build();
    let mut req = HttpSession::new_http1(Box::new(mock_io));
    req.read_request().await.unwrap();
    assert_eq!(input.as_slice(), req.to_h1_raw());

    let (mut subreq, _handle) = create_session(&req);
    subreq.read_request().await.unwrap();
    assert_eq!(input.as_slice(), subreq.to_h1_raw());
}
