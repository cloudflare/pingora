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

use once_cell::sync::Lazy;
use pingora_core::protocols::http::SERVER_NAME;

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

async fn write_purge_response(
    session: &mut Session,
    resp: &ResponseHeader,
) -> (bool, Option<Box<Error>>) {
    match session.as_mut().write_response_header_ref(resp).await {
        Ok(_) => (true, None),
        // dirty, not reusable
        Err(e) => (false, Some(e.into_down())),
    }
}

/// Write a response for a rejected cache purge requests
pub async fn write_no_purge_response(session: &mut Session) -> (bool, Option<Box<Error>>) {
    // TODO: log send error
    write_purge_response(session, &NOT_PURGEABLE).await
}

static OK: Lazy<ResponseHeader> = Lazy::new(|| gen_purge_response(200));
static NOT_FOUND: Lazy<ResponseHeader> = Lazy::new(|| gen_purge_response(404));
// for when purge is sent to uncacheable assets
static NOT_PURGEABLE: Lazy<ResponseHeader> = Lazy::new(|| gen_purge_response(405));

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
        match session.cache.purge().await {
            Ok(found) => {
                // canned PURGE response based on whether we found the asset or not
                let resp = if found { &*OK } else { &*NOT_FOUND };
                let (reuse, err) = write_purge_response(session, resp).await;
                if let Some(e) = err.as_ref() {
                    error!(
                        "Failed to send purge response: {}, {}",
                        e,
                        self.inner.request_summary(session, ctx)
                    )
                }
                Some((reuse, err))
            }
            Err(e) => {
                session.cache.disable(NoCacheReason::StorageError);
                warn!(
                    "Fail to purge cache: {}, {}",
                    e,
                    self.inner.request_summary(session, ctx)
                );
                session.downstream_session.respond_error(500).await;
                // still reusable
                Some((true, Some(e)))
            }
        }
    }
}
