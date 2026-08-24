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

//! Holding the final upstream response header until the application commits it.
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
    /// Committed by the application. The header, and the committed body unless
    /// the proxy moved it into the current body task, go out ahead of that task.
    Released {
        header: Box<ResponseHeader>,
        body: Option<Bytes>,
    },
    /// The committed header was queued downstream.
    Sent,
    /// A final header passed through without being held.
    Bypassed,
    /// Committing failed. Every later task fails.
    Failed,
}

impl<DS> Session<DS>
where
    DS: DownstreamSession,
{
    /// The held final header, after `response_filter` and per-protocol framing.
    /// Keep Content-Length and Transfer-Encoding consistent with any body change.
    ///
    /// Experimental: this API may change or be removed until indicated otherwise.
    pub fn held_response_header_mut(&mut self) -> Option<&mut ResponseHeader> {
        match &mut self.response_hold {
            ResponseHoldState::Held(header) => Some(header),
            _ => None,
        }
    }

    /// Let the proxy send the held header, then `body`, ahead of the current
    /// body chunk or trailers. `body` and later chunks go through the normal
    /// downstream body filters. Returns an error if no header is held, the
    /// header is now 1xx, or a response was written while it was held. That
    /// fails the request, except on a request that did not opt in, whose state
    /// is left alone.
    ///
    /// Experimental: this API may change or be removed until indicated otherwise.
    pub fn commit_held_response_header(&mut self, body: Option<Bytes>) -> Result<()> {
        if matches!(self.response_hold, ResponseHoldState::Immediate) {
            return Error::e_explain(InternalError, "no response header is held");
        }
        let ResponseHoldState::Held(header) =
            std::mem::replace(&mut self.response_hold, ResponseHoldState::Failed)
        else {
            return Error::e_explain(InternalError, "no response header is held");
        };
        if header.status.is_informational() {
            return Error::e_explain(
                InternalError,
                "a committed response header must be a final, non-1xx response",
            );
        }
        if self.response_write_started() {
            return Error::e_explain(
                InternalError,
                "a response was written while its header was held",
            );
        }
        self.response_hold = ResponseHoldState::Released { header, body };
        Ok(())
    }

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

    /// Also true after a failed commit, so the body filters never see held bytes.
    pub(crate) fn response_header_held(&self) -> bool {
        matches!(
            self.response_hold,
            ResponseHoldState::Held(_) | ResponseHoldState::Failed
        )
    }

    pub(crate) fn response_hold_committed(&self) -> bool {
        matches!(self.response_hold, ResponseHoldState::Released { .. })
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
                "upstream response ended before a held header was committed",
            )
        })
    }

    /// Queue `task` for the downstream, holding the final header and sending
    /// a committed one first.
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
                    // Informational responses go out before the body filters run.
                    if !tasks.is_empty() && self.write_response_tasks(std::mem::take(tasks)).await?
                    {
                        return Error::e_explain(
                            InternalError,
                            "response ended before its held header was committed",
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
                HttpTask::Body(None, false) if !self.response_write_started() => {}
                HttpTask::Failed(_) => tasks.push(task),
                _ => {
                    return Error::e_explain(
                        InternalError,
                        "response arrived before its held header was committed",
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
                    "the held response header could not be committed",
                );
            }
            ResponseHoldState::Immediate
            | ResponseHoldState::Sent
            | ResponseHoldState::Bypassed => tasks.push(task),
        }
        Ok(())
    }
}

impl<SV, C, DS> HttpProxy<SV, C, DS>
where
    C: custom::Connector,
    DS: DownstreamSession,
{
    /// Route a body passed at commit through the normal downstream body
    /// filters, after an upstream filter committed the held header.
    pub(crate) async fn committed_body_filter(
        &self,
        session: &mut Session<DS>,
        task: &mut HttpTask,
        duration: Option<Duration>,
        ctx: &mut <SV as ProxyHttp<DS>>::CTX,
    ) -> Result<Option<Duration>>
    where
        SV: ProxyHttp<DS> + Send + Sync,
        <SV as ProxyHttp<DS>>::CTX: Send + Sync,
    {
        let ResponseHoldState::Released {
            body: committed, ..
        } = &mut session.response_hold
        else {
            return Ok(duration);
        };
        match task {
            HttpTask::Body(Some(_), _) => {
                session.response_hold = ResponseHoldState::Failed;
                Error::e_explain(
                    InternalError,
                    "a body filter that commits must pass the body to the commit",
                )
            }
            // The committed body continues as this task, through the normal filters.
            HttpTask::Body(body, _) => {
                *body = committed.take();
                Ok(duration)
            }
            HttpTask::Trailer(_) => {
                let mut body = committed.take();
                if body.is_none() {
                    return Ok(None);
                }
                // The committed body skips the per-protocol response filters, so run
                // the body filter here. Range filtering only applies to cached responses.
                let duration = self
                    .inner
                    .response_body_filter(session, &mut body, false, ctx)
                    .await?;
                if let ResponseHoldState::Released { body: pending, .. } =
                    &mut session.response_hold
                {
                    *pending = body;
                }
                Ok(duration)
            }
            _ => Ok(duration),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora_core::upstreams::peer::HttpPeer;
    use tokio::io::AsyncWriteExt;

    async fn new_request_session() -> Session {
        let (mut client, server) = tokio::io::duplex(1024);
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();
        // Drain the response so direct writes succeed.
        tokio::spawn(async move { tokio::io::copy(&mut client, &mut tokio::io::sink()).await });
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
    async fn failed_commit_blocks_retry_and_later_tasks() {
        // A request that did not opt in keeps its state.
        let mut session = new_request_session().await;
        assert!(session.held_response_header_mut().is_none());
        assert!(session.commit_held_response_header(None).is_err());
        assert!(!session.response_hold_gated());

        // A commit with nothing held fails the request.
        session.arm_response_hold(true);
        assert!(session.commit_held_response_header(None).is_err());
        assert!(matches!(session.response_hold, ResponseHoldState::Failed));

        let mut session = new_request_session().await;
        session.arm_response_hold(true);
        let mut tasks = Vec::new();
        session
            .push_held_task(HttpTask::Header(header(200), false), &mut tasks)
            .await
            .unwrap();

        let header = session.held_response_header_mut().unwrap();
        header.set_status(103).unwrap();
        assert!(session.commit_held_response_header(None).is_err());

        assert!(matches!(session.response_hold, ResponseHoldState::Failed));
        assert!(session.response_header_held());
        assert!(session.response_hold_blocks_retry());
        session.rearm_response_hold();
        assert!(session.response_hold_blocks_retry());
        assert!(session.commit_held_response_header(None).is_err());
        for task in [
            HttpTask::Body(Some(Bytes::from_static(b"x")), false),
            HttpTask::Done,
        ] {
            assert!(session.push_held_task(task, &mut tasks).await.is_err());
        }
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn direct_write_while_held_fails() {
        for commit in [false, true] {
            let mut session = new_request_session().await;
            session.arm_response_hold(true);
            let mut tasks = Vec::new();
            session
                .push_held_task(HttpTask::Header(header(200), false), &mut tasks)
                .await
                .unwrap();
            session.respond_error(500).await.unwrap();
            if commit {
                assert!(session.commit_held_response_header(None).is_err());
            } else {
                let task = HttpTask::Body(None, false);
                assert!(session.push_held_task(task, &mut tasks).await.is_err());
            }
            assert!(session.response_hold_blocks_retry());
        }
    }

    /// Commits from the body filter, leaving `ctx` in the body chunk.
    struct Committer;

    #[async_trait]
    impl ProxyHttp for Committer {
        type CTX = Option<Bytes>;

        fn new_ctx(&self) -> Self::CTX {
            None
        }

        async fn upstream_peer(
            &self,
            _session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            unreachable!("tests call upstream_filter directly")
        }

        async fn upstream_response_body_filter(
            &self,
            session: &mut Session,
            body: &mut Option<Bytes>,
            _end_of_stream: bool,
            ctx: &mut Self::CTX,
        ) -> Result<Option<Duration>> {
            session.commit_held_response_header(body.take())?;
            *body = ctx.take();
            Ok(None)
        }
    }

    #[tokio::test]
    async fn body_left_alongside_a_commit_fails() {
        let proxy = HttpProxy::new(Committer, Arc::new(ServerConf::default()));
        let mut session = new_request_session().await;
        session.arm_response_hold(true);
        let mut tasks = Vec::new();
        session
            .push_held_task(HttpTask::Header(header(200), false), &mut tasks)
            .await
            .unwrap();
        let mut task = HttpTask::Body(Some(Bytes::from_static(b"held")), false);
        let mut ctx = Some(Bytes::from_static(b"more"));
        assert!(proxy
            .upstream_filter(&mut session, &mut task, &mut ctx)
            .await
            .is_err());
        assert!(matches!(session.response_hold, ResponseHoldState::Failed));
    }
}
