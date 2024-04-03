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

use super::*;
use pingora_core::protocols::http::error_resp;
use std::borrow::Cow;

#[derive(Debug)]
pub enum PurgeStatus {
    /// Cache was not enabled, purge ineffectual.
    NoCache,
    /// Asset was found in cache (and presumably purged or being purged).
    Found,
    /// Asset was not found in cache.
    NotFound,
    /// Cache returned a purge error.
    /// Contains causing error in case it should affect the downstream response.
    Error(Box<Error>),
}

// Return a canned response to a purge request, based on whether the cache had the asset or not
// (or otherwise returned an error).
fn purge_response(purge_status: &PurgeStatus) -> Cow<'static, ResponseHeader> {
    let resp = match purge_status {
        PurgeStatus::NoCache => &*NOT_PURGEABLE,
        PurgeStatus::Found => &*OK,
        PurgeStatus::NotFound => &*NOT_FOUND,
        PurgeStatus::Error(ref _e) => &*INTERNAL_ERROR,
    };
    Cow::Borrowed(resp)
}

fn gen_purge_response(code: u16) -> ResponseHeader {
    let mut resp = ResponseHeader::build(code, Some(3)).unwrap();
    resp.insert_header(header::SERVER, &SERVER_NAME[..])
        .unwrap();
    resp.insert_header(header::CONTENT_LENGTH, 0).unwrap();
    resp.insert_header(header::CACHE_CONTROL, "private, no-store")
        .unwrap();
    // TODO more headers?
    resp
}

static OK: Lazy<ResponseHeader> = Lazy::new(|| gen_purge_response(200));
static NOT_FOUND: Lazy<ResponseHeader> = Lazy::new(|| gen_purge_response(404));
// for when purge is sent to uncacheable assets
static NOT_PURGEABLE: Lazy<ResponseHeader> = Lazy::new(|| gen_purge_response(405));
// on cache storage or proxy error
static INTERNAL_ERROR: Lazy<ResponseHeader> = Lazy::new(|| error_resp::gen_error_response(500));

impl<SV> HttpProxy<SV> {
    pub(crate) async fn proxy_purge(
        &self,
        session: &mut Session,
        ctx: &mut SV::CTX,
    ) -> Option<(bool, Option<Box<Error>>)>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        let purge_status = if session.cache.enabled() {
            match session.cache.purge().await {
                Ok(found) => {
                    if found {
                        PurgeStatus::Found
                    } else {
                        PurgeStatus::NotFound
                    }
                }
                Err(e) => {
                    session.cache.disable(NoCacheReason::StorageError);
                    warn!(
                        "Fail to purge cache: {e}, {}",
                        self.inner.request_summary(session, ctx)
                    );
                    PurgeStatus::Error(e)
                }
            }
        } else {
            // cache was not enabled
            PurgeStatus::NoCache
        };

        let mut purge_resp = purge_response(&purge_status);
        if let Err(e) =
            self.inner
                .purge_response_filter(session, ctx, purge_status, &mut purge_resp)
        {
            error!(
                "Failed purge response filter: {e}, {}",
                self.inner.request_summary(session, ctx)
            );
            purge_resp = Cow::Borrowed(&*INTERNAL_ERROR)
        }

        let write_result = match purge_resp {
            Cow::Borrowed(r) => session.as_mut().write_response_header_ref(r).await,
            Cow::Owned(r) => session.as_mut().write_response_header(Box::new(r)).await,
        };
        let (reuse, err) = match write_result {
            Ok(_) => (true, None),
            // dirty, not reusable
            Err(e) => {
                let e = e.into_down();
                error!(
                    "Failed to send purge response: {e}, {}",
                    self.inner.request_summary(session, ctx)
                );
                (false, Some(e))
            }
        };
        Some((reuse, err))
    }
}
