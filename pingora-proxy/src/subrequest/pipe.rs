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

//! Subrequest piping.
//!
//! Along with subrequests themselves, subrequest piping as a feature is in
//! alpha stages, APIs are highly unstable and subject to change at any point.
//!
//! Unlike proxy_*, it is not a "true" proxy mode; the functions here help
//! establish a pipe between the main downstream session and the subrequest (which
//! in most cases will be used as a downstream session itself).
//!
//! Furthermore, only downstream modules are invoked on the main downstream session,
//! and the ProxyHttp trait filters are not run on the HttpTasks from the main session
//! (the only relevant one being the request body filter).

use crate::proxy_common::{DownstreamStateMachine, ResponseStateMachine};
use crate::subrequest::*;
use crate::{DownstreamSession, PreparedSubrequest, Session};
use bytes::Bytes;
use futures::FutureExt;
use log::{debug, warn};
use pingora_core::protocols::http::{subrequest::server::SubrequestHandle, HttpTask};
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use tokio::sync::mpsc;

pub enum InputBodyType {
    /// Preset body
    Preset(InputBody),
    /// Body should be saved (up to limit)
    SaveBody(usize),
}

/// Outcome of [`pipe_subrequest`].
#[derive(Debug, Default)]
pub struct PipeSubrequestState {
    /// Captured body from the main session.
    pub saved_body: Option<SavedBody>,
    /// Did the subrequest produce a response header? Checked before the task
    /// filter runs, so a filtered-out header still counts.
    pub header_received: bool,
    /// The spawned subrequest task handle. Always set after spawn. Caller is
    /// responsible for awaiting/inspecting state.
    pub join_handle: Option<tokio::task::JoinHandle<()>>,
    /// The receiving half of the pipe channel. When the coordinator exits
    /// `pipe_subrequest` before the subrequest task finishes writing, this
    /// receiver must be kept alive and drained alongside the join handle;
    /// otherwise dropping it breaks the pipe and prevents the writer from
    /// completing its cache-write lifecycle.
    pub pipe_rx: Option<mpsc::Receiver<HttpTask>>,
}

impl PipeSubrequestState {
    /// Creates a snapshot for error reporting, excluding the join handle.
    /// Moves `pipe_rx` into the snapshot so the receiver stays alive through
    /// the error path and is not dropped when `self` is cleaned up.
    /// Used by [`map_pipe_err`] to capture state at the point of failure.
    pub fn snapshot_for_error(&mut self) -> Self {
        PipeSubrequestState {
            saved_body: self.saved_body.clone(),
            header_received: self.header_received,
            join_handle: None,
            pipe_rx: self.pipe_rx.take(),
        }
    }
}

pub struct PipeSubrequestError {
    pub state: PipeSubrequestState,
    /// Whether error originated (and was propagated from) subrequest itself
    /// (vs. an error that occurred while sending task)
    pub from_subreq: bool,
    pub error: Box<Error>,
}
impl PipeSubrequestError {
    pub fn new(
        error: impl Into<Box<Error>>,
        from_subreq: bool,
        state: PipeSubrequestState,
    ) -> Self {
        PipeSubrequestError {
            error: error.into(),
            from_subreq,
            state,
        }
    }
}

fn map_pipe_err<T, E: Into<Box<Error>>>(
    result: Result<T, E>,
    from_subreq: bool,
    state: &mut PipeSubrequestState,
) -> Result<T, PipeSubrequestError> {
    result.map_err(|e| PipeSubrequestError::new(e, from_subreq, state.snapshot_for_error()))
}

#[derive(Debug, Clone)]
pub struct SavedBody {
    body: Vec<Bytes>,
    complete: bool,
    truncated: bool,
    length: usize,
    max_length: usize,
}

impl SavedBody {
    pub fn new(max_length: usize) -> Self {
        SavedBody {
            body: vec![],
            complete: false,
            truncated: false,
            length: 0,
            max_length,
        }
    }

    pub fn save_body_bytes(&mut self, body_bytes: Bytes) -> bool {
        let len = body_bytes.len();
        if self.length + len > self.max_length {
            self.truncated = true;
            return false;
        }
        self.length += len;
        self.body.push(body_bytes);
        true
    }

    pub fn is_body_complete(&self) -> bool {
        self.complete && !self.truncated
    }

    pub fn set_body_complete(&mut self) {
        self.complete = true;
    }
}

#[derive(Debug, Clone)]
pub enum InputBody {
    NoBody,
    Bytes(Vec<Bytes>),
    // TODO: stream
}

impl InputBody {
    pub(crate) fn into_reader(self) -> InputBodyReader {
        InputBodyReader(match self {
            InputBody::NoBody => vec![].into_iter(),
            InputBody::Bytes(v) => v.into_iter(),
        })
    }

    pub fn is_body_empty(&self) -> bool {
        match self {
            InputBody::NoBody => true,
            InputBody::Bytes(v) => v.is_empty(),
        }
    }
}

impl std::convert::From<SavedBody> for InputBody {
    fn from(body: SavedBody) -> Self {
        if body.body.is_empty() {
            InputBody::NoBody
        } else {
            InputBody::Bytes(body.body)
        }
    }
}

/// Pipe the main session's request into `subrequest` and the subrequest's response back to the
/// main session.
///
/// When [`Session::handles_downstream_abandonment`] is set, the pipe also watches the client
/// while it waits for the subrequest's response, including while the subrequest has not asked
/// for a body, is fed a preset one or cannot take more input: an HTTP/2 client from the start,
/// any other client once its request body is complete. It fails with the downstream error as
/// soon as the client leaves instead of waiting for that response. The watch is the regular idle
/// read, as in the regular proxy: an HTTP/1 next request that the client sends early fails the
/// pipe with a downstream `ConnectError` while pipelining is disabled, and is kept for the next
/// session when it is enabled (see
/// [`set_pipelining_enabled`](pingora_core::protocols::http::ServerSession::set_pipelining_enabled)).
/// Once the watch has kept that request's first byte, later idle reads do not read the client,
/// so a client that then closes goes unnoticed until the pipe ends, as in the proxy loops. The
/// watch stops once the response is complete, so a next request sent after it stays unread.
/// Without the opt-in, the pipe reads the client only to forward a body the subrequest asked
/// for, so an early next request can be left unread.
///
/// With or without the opt-in, a client read that fails the pipe turns keep-alive off: an
/// HTTP/1 read after the request body may have consumed the start of the next request, so the
/// connection must not be reused.
pub async fn pipe_subrequest<DS, F>(
    session: &mut Session<DS>,
    mut subrequest: PreparedSubrequest<DS>,
    subrequest_handle: SubrequestHandle,
    mut task_filter: F,
    input_body: InputBodyType,
) -> std::result::Result<PipeSubrequestState, PipeSubrequestError>
where
    DS: DownstreamSession,
    F: FnMut(HttpTask) -> Result<Option<HttpTask>>,
{
    let (maybe_preset_body, saved_body) = match input_body {
        InputBodyType::Preset(body) => (Some(body), None),
        InputBodyType::SaveBody(limit) => (None, Some(SavedBody::new(limit))),
    };
    let use_preset_body = maybe_preset_body.is_some();

    let mut response_state = ResponseStateMachine::new();
    let (no_body_input, mut maybe_preset_reader) = if use_preset_body {
        let preset_body = maybe_preset_body.expect("checked above");
        (preset_body.is_body_empty(), Some(preset_body.into_reader()))
    } else {
        (session.as_mut().is_body_done(), None)
    };
    let mut downstream_state = DownstreamStateMachine::new(no_body_input);

    let mut state = PipeSubrequestState {
        saved_body,
        ..Default::default()
    };

    // Remove headers if no body.
    let join_handle = tokio::spawn(async move {
        if no_body_input {
            subrequest
                .session_mut()
                .as_subrequest_mut()
                .expect("PreparedSubrequest must be subrequest")
                .clear_request_body_headers();
        }
        let _ = subrequest.run().await;
    });
    state.join_handle = Some(join_handle);
    let tx = subrequest_handle.tx;
    // Move rx into state immediately so it survives all exit paths (early `?`
    // returns, errors, and the normal success path). The select loop borrows it
    // back via `state.pipe_rx.as_mut().expect(...)`.
    state.pipe_rx = Some(subrequest_handle.rx);

    let mut wants_body = false;
    let mut wants_body_rx_err = false;
    let mut wants_body_rx = subrequest_handle.subreq_wants_body;

    let mut proxy_error_rx_err = false;
    let mut proxy_error_rx = subrequest_handle.subreq_proxy_error;

    // Whether the pipe may watch the client; see `watch_downstream` below. Cleared once an idle
    // wait ends without an error.
    let mut may_watch_downstream = session.handles_downstream_abandonment();

    // Note: "upstream" here refers to subrequest session tasks,
    // downstream refers to main session
    while !downstream_state.is_done() || !response_state.is_done() {
        let send_permit = tx
            .try_reserve()
            .or_err(InternalError, "try_reserve() body pipe for subrequest");
        let forward_body = wants_body && !use_preset_body && send_permit.is_ok();
        // With the opt-in, the pipe watches the main session's client whenever it waits for the
        // subrequest's response without reading that client to forward a body;
        // `pipe_subrequest` documents the policy. The protocol that matters is the main
        // session's, not the subrequest's, which is always HTTP/1. An HTTP/2 main session's idle
        // wait reads nothing, so it is watched at once. Any other idle read takes a byte, which
        // belongs to the request body until the body is complete, so that client is watched
        // only then. Without a preset body, only the forwarding read below and a response write
        // complete the body, and each marks the downstream done in the same branch, so by then
        // the pipe has already sent the body's end. `!forward_body` keeps the watch off while
        // the forwarding read, which once the body is complete is this same idle read, reads
        // the client.
        let watch_downstream = may_watch_downstream
            && !forward_body
            && !response_state.upstream_delivery_done()
            && (session.is_http2() || session.is_body_done());

        tokio::select! {
            task = state.pipe_rx.as_mut().expect("pipe_rx always set after spawn").recv(), if !response_state.upstream_delivery_done() => {
                debug!("upstream event: {:?}", task);
                if let Some(t) = task {
                    // Did the subrequest get headers?
                    if matches!(&t, HttpTask::Header(..)) {
                        state.header_received = true;
                    }
                    // pull as many tasks as we can
                    const TASK_BUFFER_SIZE: usize = 4;
                    let mut tasks = Vec::with_capacity(TASK_BUFFER_SIZE);
                    let task = map_pipe_err(task_filter(t), false, &mut state)?;
                    if let Some(filtered) = task {
                        tasks.push(filtered);
                    }
                    // tokio::task::unconstrained because now_or_never may yield None when the future is ready
                    while let Some(maybe_task) = tokio::task::unconstrained(state.pipe_rx.as_mut().expect("pipe_rx always set after spawn").recv()).now_or_never() {
                        if let Some(t) = maybe_task {
                            if matches!(&t, HttpTask::Header(..)) {
                                state.header_received = true;
                            }
                            let task = map_pipe_err(task_filter(t), false, &mut state)?;
                            if let Some(filtered) = task {
                                tasks.push(filtered);
                            }
                        } else {
                            break
                        }
                    }
                    // FIXME: if one of these tasks is Failed(e), the session will return that
                    // error; in this case, the error is actually from the subreq
                    let response_done = map_pipe_err(session.write_response_tasks(tasks).await, false, &mut state)?;

                    // NOTE: technically it is the downstream whose response state has finished here
                    // we consider the subrequest's work done however
                    response_state.maybe_set_upstream_delivery_done(response_done);
                    // unsuccessful upgrade response may force the request done
                    // (can only happen with a real session, TODO to allow with preset body)
                    downstream_state.maybe_finished(!use_preset_body && session.is_body_done());
                } else {
                    debug!("upstream channel closed early");
                    response_state.maybe_set_upstream_delivery_done(true);
                }
            },

            res = &mut wants_body_rx, if !wants_body && !wants_body_rx_err => {
                // subrequest may need time before it needs body, or it may not actually require it
                // TODO: tx send permit may not be necessary if no oneshot exists
                if res.is_err() {
                    wants_body_rx_err = true;
                } else {
                    wants_body = true;
                }
            }

            res = &mut proxy_error_rx, if !proxy_error_rx_err => {
                if let Ok(e) = res {
                    // propagate proxy error to caller
                    return Err(PipeSubrequestError::new(e, true, state));
                } else {
                    // subrequest dropped, let select loop finish
                    proxy_error_rx_err = true;
                }
            }

            _ = tx.reserve(), if downstream_state.is_reading() && send_permit.is_err() => {
                // If tx is closed, the upstream has already finished its job.
                downstream_state.maybe_finished(tx.is_closed());
                debug!("waiting for permit {send_permit:?}, upstream closed {}", tx.is_closed());
                /* No permit, wait on more capacity to avoid starving.
                 * Otherwise this select only blocks on rx, which might send no data
                 * before the entire body is uploaded.
                 * once more capacity arrives we just loop back
                 */
            },

            body = session.downstream_session.read_body_or_idle(watch_downstream || downstream_state.is_done()),
                if downstream_state.can_poll() && (forward_body || watch_downstream) => {
                debug!(
                    "downstream event: {} for subrequest",
                    if watch_downstream { "client watch" } else { "main body" }
                );
                if let Err(e) = body.as_ref() {
                    // A client whose read failed is not reused: an HTTP/1 client read after the
                    // request body may have consumed the start of the next request.
                    debug!("downstream read failed, keep-alive off: {e}");
                    session.downstream_session.set_keepalive(None);
                }
                let body = map_pipe_err(body.map_err(|e| e.into_down()), false, &mut state)?;
                // The watch reserves no send permit and yields no client body, so it must not
                // fall through to the forwarding path below. Only a custom session can end its
                // idle wait without an error; stop watching instead of waiting on it again.
                if watch_downstream {
                    may_watch_downstream = false;
                    continue;
                }

                // this is the first subrequest
                // send the body

                // If the request is websocket, `None` body means the request is closed.
                // Set the response to be done as well so that the request completes normally.
                if body.is_none() && session.is_upgrade_req() {
                    response_state.maybe_set_upstream_delivery_done(true);
                }

                let is_body_done = session.is_body_done();
                let request_done = map_pipe_err(send_body_to_pipe(
                    session,
                    body,
                    is_body_done,
                    state.saved_body.as_mut(),
                    send_permit.expect("checked is_ok()"),
                )
                .await, false, &mut state)?;

                downstream_state.maybe_finished(request_done);

            },

            // lazily evaluated async block allows us to expect() inside the select! branch
            body = async { maybe_preset_reader.as_mut().expect("preset body set").read_body() },
                if wants_body && use_preset_body && !downstream_state.is_done() && downstream_state.can_poll() && send_permit.is_ok() => {
                debug!("downstream event: preset body for subrequest");

                // TODO: WebSocket handling to set upstream done?

                // preset None body indicates we are done
                let is_body_done = body.is_none();
                // Don't run downstream modules on preset input body
                let request_done = map_pipe_err(do_send_body_to_pipe(
                    body,
                    is_body_done,
                    None,
                    send_permit.expect("checked is_ok()"),
                ), false, &mut state)?;
                downstream_state.maybe_finished(request_done);

            },

            else => break,
        }
    }
    // The output channel can close in the same poll that publishes a proxy error.
    // Reconcile the terminal error before reporting successful pipe completion.
    if let Ok(e) = proxy_error_rx.try_recv() {
        return Err(PipeSubrequestError::new(e, true, state));
    }
    Ok(state)
}

// Mostly the same as proxy_common, but does not run proxy request_body_filter
async fn send_body_to_pipe<DS>(
    session: &mut Session<DS>,
    mut data: Option<Bytes>,
    end_of_body: bool,
    saved_body: Option<&mut SavedBody>,
    tx: mpsc::Permit<'_, HttpTask>,
) -> Result<bool>
where
    DS: DownstreamSession,
{
    // None: end of body
    // this var is to signal if downstream finish sending the body, which shouldn't be
    // affected by the request_body_filter
    let end_of_body = end_of_body || data.is_none();

    session
        .downstream_modules_ctx
        .request_body_filter(&mut data, end_of_body)
        .await?;

    do_send_body_to_pipe(data, end_of_body, saved_body, tx)
}

fn do_send_body_to_pipe(
    data: Option<Bytes>,
    end_of_body: bool,
    mut saved_body: Option<&mut SavedBody>,
    tx: mpsc::Permit<'_, HttpTask>,
) -> Result<bool> {
    // the flag to signal to upstream
    let upstream_end_of_body = end_of_body || data.is_none();

    /* It is normal to get 0 bytes because of multi-chunk or request_body_filter decides not to
     * output anything yet.
     * Don't write 0 bytes to the network since it will be
     * treated as the terminating chunk */
    if !upstream_end_of_body && data.as_ref().is_some_and(|d| d.is_empty()) {
        return Ok(false);
    }

    debug!(
        "Read {} bytes body from downstream",
        data.as_ref().map_or(-1, |d| d.len() as isize)
    );

    if let Some(capture) = saved_body.as_mut() {
        if capture.is_body_complete() {
            warn!("subrequest trying to save body after body is complete");
        } else if let Some(d) = data.as_ref() {
            capture.save_body_bytes(d.clone());
        }
        if end_of_body {
            capture.set_body_complete();
        }
    }

    tx.send(HttpTask::Body(data, upstream_end_of_body));

    Ok(end_of_body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subrequest::Ctx as SubrequestCtx;
    use crate::{Session, Subrequest, SubrequestSpawner};
    use async_trait::async_trait;
    use pingora_core::protocols::http::ServerSession as HttpSession;
    use pingora_http::ResponseHeader;
    use std::sync::{Arc, Barrier};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Drops session without producing output — channels close, rx returns None.
    struct NoopApp;

    #[async_trait]
    impl Subrequest for NoopApp {
        async fn process_subrequest(
            self: Arc<Self>,
            _session: Box<HttpSession>,
            _ctx: Box<SubrequestCtx>,
        ) {
        }
    }

    struct HeaderThenErrorApp {
        header_selected: Arc<Barrier>,
    }

    #[async_trait]
    impl Subrequest for HeaderThenErrorApp {
        async fn process_subrequest(
            self: Arc<Self>,
            mut session: Box<HttpSession>,
            _ctx: Box<SubrequestCtx>,
        ) {
            let mut header =
                ResponseHeader::build(200, Some(1)).expect("test response header should build");
            header
                .insert_header(http::header::CONTENT_LENGTH, "0")
                .expect("test content-length should be valid");
            session
                .write_response_header(Box::new(header))
                .await
                .expect("test response header should be written");

            self.header_selected.wait();
            session.on_proxy_failure(Error::new(FileReadError));
            self.header_selected.wait();
        }
    }

    async fn mock_session() -> Session {
        let input = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";
        let mock_io = tokio_test::io::Builder::new().read(&input[..]).build();
        let mut session = Session::new_h1(Box::new(mock_io) as pingora_core::protocols::Stream);
        session
            .downstream_session
            .read_request()
            .await
            .expect("mock request should parse");
        session
    }

    async fn writable_mock_session() -> Session {
        let input = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";
        let (mut client, server) = tokio::io::duplex(1024);
        client
            .write_all(input)
            .await
            .expect("mock request should be written");
        tokio::spawn(async move {
            let mut response = Vec::new();
            let _ = client.read_to_end(&mut response).await;
        });

        let mut session = Session::new_h1(Box::new(server) as pingora_core::protocols::Stream);
        session
            .downstream_session
            .read_request()
            .await
            .expect("mock request should parse");
        session
    }

    fn hold_header_until_error(
        task: HttpTask,
        header_selected: &Barrier,
    ) -> Result<Option<HttpTask>> {
        if matches!(&task, HttpTask::Header(..)) {
            header_selected.wait();
            header_selected.wait();
        }
        Ok(Some(task))
    }

    async fn pipe_header_then_error(
    ) -> std::result::Result<PipeSubrequestState, PipeSubrequestError> {
        let header_selected = Arc::new(Barrier::new(2));
        let mut session = writable_mock_session().await;
        let app = HeaderThenErrorApp {
            header_selected: Arc::clone(&header_selected),
        };
        let spawner = SubrequestSpawner::new(Arc::new(app));
        let ctx = SubrequestCtx::builder().body_mode(BodyMode::NoBody).build();
        let (subrequest, handle) = spawner.create_subrequest(session.as_downstream(), ctx);
        pipe_subrequest(
            &mut session,
            subrequest,
            handle,
            move |task| hold_header_until_error(task, &header_selected),
            InputBodyType::Preset(InputBody::NoBody),
        )
        .await
    }

    #[tokio::test]
    async fn no_header_received_when_subrequest_exits_silently() {
        let mut session = mock_session().await;

        let spawner = SubrequestSpawner::new(Arc::new(NoopApp));
        let ctx = SubrequestCtx::builder().body_mode(BodyMode::NoBody).build();
        let (subrequest, handle) = spawner.create_subrequest(session.as_downstream(), ctx);

        let result = pipe_subrequest(
            &mut session,
            subrequest,
            handle,
            |task| Ok(Some(task)),
            InputBodyType::Preset(InputBody::NoBody),
        )
        .await;

        let state =
            result.unwrap_or_else(|e| panic!("pipe should return Ok, not Err: {:?}", e.error));
        assert!(
            !state.header_received,
            "no header should have been received from the no-op subrequest"
        );
        assert!(state.join_handle.is_some(), "task handle should be set");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preserves_proxy_error_queued_before_pipe_completion() {
        let error = pipe_header_then_error()
            .await
            .expect_err("subrequest proxy error should be preserved");
        assert!(error.from_subreq);
        assert_eq!(error.error.root_etype(), &FileReadError);
        assert!(error.state.header_received);
    }
}

#[cfg(test)]
mod disconnect_tests;
