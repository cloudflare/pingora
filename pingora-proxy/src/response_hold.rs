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

//! Holding the final upstream response header until a body verdict releases it.
//!
//! See [`ProxyHttp::hold_response_header`]. A request that does not opt in
//! stays `Immediate`, and each proxy call site checks that before anything else.

use super::*;
use pingora_core::protocols::http::custom::is_informational_except_101;

pub(crate) enum ResponseHoldState {
    Immediate,
    /// Waiting for the final upstream header.
    Armed,
    Held(Box<ResponseHeader>),
    /// Released by the application. The header, and the released body unless
    /// the proxy moved it into the current body task, go out ahead of that task.
    Released {
        header: Box<ResponseHeader>,
        body: Option<Bytes>,
    },
    /// The released header was queued downstream.
    Sent,
    /// A final header passed through without being held.
    Bypassed,
    /// Releasing failed. Every later task fails.
    Failed,
}

/// The held final header of an upstream response.
///
/// It gives read access to the session and write access to the held header.
/// [`Self::release`] consumes it and is the only way to send body bytes.
///
/// Experimental: this API may change or be removed until indicated otherwise.
pub struct HeldResponse<'a, DS = ()>
where
    DS: DownstreamSession,
{
    session: &'a mut Session<DS>,
}

impl<DS> HeldResponse<'_, DS>
where
    DS: DownstreamSession,
{
    /// Read-only access to the session.
    pub fn session(&self) -> &Session<DS> {
        self.session
    }

    /// The held header, after `response_filter` and per-protocol framing. Keep
    /// Content-Length and Transfer-Encoding consistent with any body change.
    pub fn header_mut(&mut self) -> &mut ResponseHeader {
        let ResponseHoldState::Held(header) = &mut self.session.response_hold else {
            unreachable!("HeldResponse requires a held header")
        };
        header
    }

    /// Let the proxy send the held header, then `body`, ahead of the current
    /// body chunk or trailers. `body` and later chunks go through the normal
    /// downstream body filters. Fails, and fails the request, if the header is
    /// now 1xx.
    pub fn release(self, body: Option<Bytes>) -> Result<()> {
        match std::mem::replace(&mut self.session.response_hold, ResponseHoldState::Failed) {
            ResponseHoldState::Held(header) if !header.status.is_informational() => {
                self.session.response_hold = ResponseHoldState::Released { header, body };
                Ok(())
            }
            _ => Error::e_explain(
                InternalError,
                "a released response header must be a final, non-1xx response",
            ),
        }
    }
}

impl<DS> Session<DS>
where
    DS: DownstreamSession,
{
    pub(crate) fn arm_response_hold(&mut self, hold: bool) {
        if hold {
            self.response_hold = ResponseHoldState::Armed;
        }
    }

    /// Disable caching for a held request so every response reaches the
    /// upstream response filters.
    pub(crate) fn bypass_cache_for_response_hold(&mut self) {
        if !matches!(self.response_hold, ResponseHoldState::Immediate)
            && (self.cache.enabled() || self.cache.bypassing())
        {
            self.cache
                .disable(NoCacheReason::Custom("response header hold"));
        }
    }

    /// Whether response tasks must go through [`Self::push_held_task`].
    pub(crate) fn response_hold_gated(&self) -> bool {
        matches!(
            self.response_hold,
            ResponseHoldState::Armed
                | ResponseHoldState::Held(_)
                | ResponseHoldState::Released { .. }
                | ResponseHoldState::Failed
        )
    }

    pub(crate) fn response_header_held(&self) -> bool {
        matches!(self.response_hold, ResponseHoldState::Held(_))
    }

    fn final_header_written(&self) -> bool {
        self.response_written()
            .is_some_and(|header| !is_informational_except_101(header.status.as_u16() as u32))
    }

    /// Whether a final header or any body was written or queued downstream.
    fn response_write_started(&self) -> bool {
        self.final_header_written()
            || self.body_bytes_sent() > 0
            || self.has_pending_downstream_tasks()
    }

    /// Once the application has seen the held header, its verdict belongs to
    /// this attempt. A header that passed through blocks retry once written.
    pub(crate) fn response_hold_blocks_retry(&self) -> bool {
        match self.response_hold {
            ResponseHoldState::Immediate => false,
            ResponseHoldState::Armed | ResponseHoldState::Bypassed => self.final_header_written(),
            _ => true,
        }
    }

    pub(crate) fn rearm_response_hold(&mut self) {
        if matches!(self.response_hold, ResponseHoldState::Bypassed) {
            self.response_hold = ResponseHoldState::Armed;
        }
    }

    /// The error for an attempt that ended without error but left the hold unfinished.
    pub(crate) fn unfinished_hold_error(&self) -> Option<Box<Error>> {
        let unfinished = match self.response_hold {
            ResponseHoldState::Immediate
            | ResponseHoldState::Sent
            | ResponseHoldState::Bypassed => false,
            ResponseHoldState::Armed => !self.final_header_written(),
            _ => true,
        };
        unfinished.then(|| {
            Error::explain(
                InternalError,
                "upstream response ended before a held header was released",
            )
        })
    }

    /// Queue `task` for the downstream, holding the final header and sending
    /// a released one first.
    pub(crate) async fn push_held_task(
        &mut self,
        task: HttpTask,
        tasks: &mut Vec<HttpTask>,
    ) -> Result<()> {
        match &self.response_hold {
            ResponseHoldState::Armed => match task {
                HttpTask::Header(header, false) if !header.status.is_informational() => {
                    if self.final_header_written() {
                        return Error::e_explain(
                            InternalError,
                            "a final response was written before its header could be held",
                        );
                    }
                    self.response_hold = ResponseHoldState::Held(header);
                    // Informational responses go out before the held filters run.
                    if !tasks.is_empty() && self.write_response_tasks(std::mem::take(tasks)).await?
                    {
                        return Error::e_explain(
                            InternalError,
                            "response ended before its held header was released",
                        );
                    }
                }
                HttpTask::Header(header, end) => {
                    if !is_informational_except_101(header.status.as_u16() as u32) {
                        self.response_hold = ResponseHoldState::Bypassed;
                    }
                    tasks.push(HttpTask::Header(header, end));
                }
                HttpTask::Failed(_) => tasks.push(task),
                _ => {
                    return Error::e_explain(
                        InternalError,
                        "response body or trailers arrived before a final response header",
                    );
                }
            },
            ResponseHoldState::Held(_) => match task {
                HttpTask::Body(None, false) => {}
                HttpTask::Failed(_) => tasks.push(task),
                _ => {
                    return Error::e_explain(
                        InternalError,
                        "response ended before its held header was released",
                    );
                }
            },
            ResponseHoldState::Released { .. } => {
                // Hooks with &mut Session, such as request_body_filter, can
                // still write a response during the hold.
                if self.response_write_started() {
                    return Error::e_explain(
                        InternalError,
                        "a response was written while its header was held",
                    );
                }
                let ResponseHoldState::Released { header, body } =
                    std::mem::replace(&mut self.response_hold, ResponseHoldState::Sent)
                else {
                    unreachable!()
                };
                tasks.push(HttpTask::Header(header, false));
                if body.is_some() {
                    tasks.push(HttpTask::Body(body, false));
                }
                tasks.push(task);
            }
            ResponseHoldState::Failed => {
                return Error::e_explain(
                    InternalError,
                    "the held response header could not be released",
                );
            }
            ResponseHoldState::Immediate
            | ResponseHoldState::Sent
            | ResponseHoldState::Bypassed => tasks.push(task),
        }
        Ok(())
    }

    /// Fail if a held filter returned with the hold in a state it may not
    /// leave it in.
    fn check_held_filter_result(&self, end_of_stream: bool) -> Result<()> {
        match self.response_hold {
            ResponseHoldState::Held(_) if end_of_stream => Error::e_explain(
                InternalError,
                "response ended before its held header was released",
            ),
            ResponseHoldState::Failed => Error::e_explain(
                InternalError,
                "the held response header could not be released",
            ),
            _ => Ok(()),
        }
    }
}

impl<SV, C, DS> HttpProxy<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    pub(crate) async fn held_body_filter(
        &self,
        session: &mut Session<DS>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<Option<Duration>>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let response = HeldResponse { session };
        let duration = self
            .inner
            .held_upstream_response_body_filter(response, body.take(), end_of_stream, ctx)
            .await?;
        session.check_held_filter_result(end_of_stream)?;
        // The released body continues as this task, through the normal filters.
        if let ResponseHoldState::Released { body: released, .. } = &mut session.response_hold {
            *body = released.take();
        }
        Ok(duration)
    }

    pub(crate) async fn held_trailer_filter(
        &self,
        session: &mut Session<DS>,
        trailers: &mut header::HeaderMap,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<Option<Duration>>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let response = HeldResponse { session };
        self.inner
            .held_upstream_response_trailer_filter(response, trailers, ctx)
            .await?;
        session.check_held_filter_result(true)?;
        let ResponseHoldState::Released { body, .. } = &mut session.response_hold else {
            unreachable!("a held trailer filter returned without releasing")
        };
        let mut body = body.take();
        if body.is_none() {
            return Ok(None);
        }
        // The handed-back body skips the per-protocol response filters, so run
        // the body filter here. Range filtering only applies to cached responses.
        let duration = self
            .inner
            .response_body_filter(session, &mut body, false, ctx)
            .await?;
        if let ResponseHoldState::Released { body: pending, .. } = &mut session.response_hold {
            *pending = body;
        }
        Ok(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn new_request_session() -> Session {
        let (mut client, server) = tokio::io::duplex(1024);
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();
        let mut session = Session::new_h1(Box::new(server) as Stream);
        session.read_request().await.unwrap();
        session
    }

    fn header(status: u16) -> Box<ResponseHeader> {
        Box::new(ResponseHeader::build(status, None).unwrap())
    }

    #[tokio::test]
    async fn retry_rearms_after_bypassed_header_but_not_after_hold() {
        let mut session = new_request_session().await;
        session.rearm_response_hold();
        assert!(matches!(
            session.response_hold,
            ResponseHoldState::Immediate
        ));
        assert!(!session.response_hold_blocks_retry());

        session.arm_response_hold(true);
        let mut tasks = Vec::new();
        session
            .push_held_task(HttpTask::Header(header(103), false), &mut tasks)
            .await
            .unwrap();
        assert!(matches!(session.response_hold, ResponseHoldState::Armed));
        session
            .push_held_task(HttpTask::Header(header(204), true), &mut tasks)
            .await
            .unwrap();
        assert!(matches!(session.response_hold, ResponseHoldState::Bypassed));
        assert!(!session.response_hold_blocks_retry());

        session.rearm_response_hold();
        tasks.clear();
        session
            .push_held_task(HttpTask::Header(header(200), false), &mut tasks)
            .await
            .unwrap();
        assert!(session.response_header_held());
        assert!(tasks.is_empty());
        assert!(session.response_hold_blocks_retry());
        assert!(session.unfinished_hold_error().is_some());
    }

    #[tokio::test]
    async fn failed_release_blocks_retry_and_later_tasks() {
        let mut session = new_request_session().await;
        session.arm_response_hold(true);
        let mut tasks = Vec::new();
        session
            .push_held_task(HttpTask::Header(header(200), false), &mut tasks)
            .await
            .unwrap();

        let mut response = HeldResponse {
            session: &mut session,
        };
        response.header_mut().set_status(103).unwrap();
        assert!(response.release(None).is_err());

        assert!(matches!(session.response_hold, ResponseHoldState::Failed));
        assert!(session.response_hold_blocks_retry());
        session.rearm_response_hold();
        assert!(session.response_hold_blocks_retry());
        assert!(session.check_held_filter_result(false).is_err());
        for task in [
            HttpTask::Body(Some(Bytes::from_static(b"x")), false),
            HttpTask::Done,
        ] {
            assert!(session.push_held_task(task, &mut tasks).await.is_err());
        }
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn released_header_goes_out_before_the_releasing_task() {
        let mut session = new_request_session().await;
        session.arm_response_hold(true);
        let mut tasks = Vec::new();
        session
            .push_held_task(HttpTask::Header(header(200), false), &mut tasks)
            .await
            .unwrap();
        HeldResponse {
            session: &mut session,
        }
        .release(Some(Bytes::from_static(b"body")))
        .unwrap();
        session
            .push_held_task(HttpTask::Trailer(None), &mut tasks)
            .await
            .unwrap();
        assert!(matches!(
            tasks.as_slice(),
            [
                HttpTask::Header(_, false),
                HttpTask::Body(Some(_), false),
                HttpTask::Trailer(None)
            ]
        ));
        assert!(!session.response_hold_gated());
        assert!(session.response_hold_blocks_retry());
        assert!(session.unfinished_hold_error().is_none());
    }
}
