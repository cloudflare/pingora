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
use crate::{PreparedSubrequest, Session};
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

/// Context struct as a result of subrequest piping.
#[derive(Clone)]
pub struct PipeSubrequestState {
    /// The saved (captured) body from the main session.
    pub saved_body: Option<SavedBody>,
}

impl PipeSubrequestState {
    fn new() -> PipeSubrequestState {
        PipeSubrequestState { saved_body: None }
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
    state: &PipeSubrequestState,
) -> Result<T, PipeSubrequestError> {
    result.map_err(|e| PipeSubrequestError::new(e, from_subreq, state.clone()))
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

pub async fn pipe_subrequest<F>(
    session: &mut Session,
    mut subrequest: PreparedSubrequest,
    subrequest_handle: SubrequestHandle,
    mut task_filter: F,
    input_body: InputBodyType,
) -> std::result::Result<PipeSubrequestState, PipeSubrequestError>
where
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

    let mut state = PipeSubrequestState::new();
    state.saved_body = saved_body;

    // Have the subrequest remove all body-related headers if no body will be sent
    // TODO: we could also await the join handle, but subrequest may be running logging phase
    // also the full run() may also await cache fill if downstream fails
    let _join_handle = tokio::spawn(async move {
        if no_body_input {
            subrequest
                .session_mut()
                .as_subrequest_mut()
                .expect("PreparedSubrequest must be subrequest")
                .clear_request_body_headers();
        }
        subrequest.run().await
    });
    let tx = subrequest_handle.tx;
    let mut rx = subrequest_handle.rx;

    let mut wants_body = false;
    let mut wants_body_rx_err = false;
    let mut wants_body_rx = subrequest_handle.subreq_wants_body;

    let mut proxy_error_rx_err = false;
    let mut proxy_error_rx = subrequest_handle.subreq_proxy_error;

    // Note: "upstream" here refers to subrequest session tasks,
    // downstream refers to main session
    while !downstream_state.is_done() || !response_state.is_done() {
        let send_permit = tx
            .try_reserve()
            .or_err(InternalError, "try_reserve() body pipe for subrequest");

        tokio::select! {
            task = rx.recv(), if !response_state.upstream_done() => {
                debug!("upstream event: {:?}", task);
                if let Some(t) = task {
                    // pull as many tasks as we can
                    const TASK_BUFFER_SIZE: usize = 4;
                    let mut tasks = Vec::with_capacity(TASK_BUFFER_SIZE);
                    let task = map_pipe_err(task_filter(t), false, &state)?;
                    if let Some(filtered) = task {
                        tasks.push(filtered);
                    }
                    // tokio::task::unconstrained because now_or_never may yield None when the future is ready
                    while let Some(maybe_task) = tokio::task::unconstrained(rx.recv()).now_or_never() {
                        if let Some(t) = maybe_task {
                            let task = map_pipe_err(task_filter(t), false, &state)?;
                            if let Some(filtered) = task {
                                tasks.push(filtered);
                            }
                        } else {
                            break
                        }
                    }
                    // FIXME: if one of these tasks is Failed(e), the session will return that
                    // error; in this case, the error is actually from the subreq
                    let response_done = map_pipe_err(session.write_response_tasks(tasks).await, false, &state)?;

                    // NOTE: technically it is the downstream whose response state has finished here
                    // we consider the subrequest's work done however
                    response_state.maybe_set_upstream_done(response_done);
                    // unsuccessful upgrade response may force the request done
                    // (can only happen with a real session, TODO to allow with preset body)
                    downstream_state.maybe_finished(!use_preset_body && session.is_body_done());
                } else {
                    // quite possible that the subrequest may be finished, though the main session
                    // is not - we still must exit in this case
                    debug!("empty upstream event");
                    response_state.maybe_set_upstream_done(true);
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

            body = session.downstream_session.read_body_or_idle(downstream_state.is_done()),
                if wants_body && !use_preset_body && downstream_state.can_poll() && send_permit.is_ok() => {
                // this is the first subrequest
                // send the body
                debug!("downstream event: main body for subrequest");
                let body = map_pipe_err(body.map_err(|e| e.into_down()), false, &state)?;

                // If the request is websocket, `None` body means the request is closed.
                // Set the response to be done as well so that the request completes normally.
                if body.is_none() && session.is_upgrade_req() {
                    response_state.maybe_set_upstream_done(true);
                }

                let is_body_done = session.is_body_done();
                let request_done = map_pipe_err(send_body_to_pipe(
                    session,
                    body,
                    is_body_done,
                    state.saved_body.as_mut(),
                    send_permit.expect("checked is_ok()"),
                )
                .await, false, &state)?;

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
                ), false, &state)?;
                downstream_state.maybe_finished(request_done);

            },

            else => break,
        }
    }
    Ok(state)
}

// Mostly the same as proxy_common, but does not run proxy request_body_filter
async fn send_body_to_pipe(
    session: &mut Session,
    mut data: Option<Bytes>,
    end_of_body: bool,
    saved_body: Option<&mut SavedBody>,
    tx: mpsc::Permit<'_, HttpTask>,
) -> Result<bool> {
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
