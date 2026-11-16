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

//! A [`pipe_subrequest`] whose main session was early-buffered must forward and save the
//! buffered body as `SaveBody` would the read one, since the downstream body was already read.

use super::*;
use crate::subrequest::{BodyMode, Ctx as SubrequestCtx};
use crate::{Subrequest, SubrequestSpawner};
use async_trait::async_trait;
use pingora_core::modules::http::{HttpModule, HttpModuleBuilder, HttpModules, Module};
use pingora_core::protocols::http::ServerSession;
use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Records the request body the subrequest receives.
struct RecordBody(Arc<Mutex<Option<Vec<u8>>>>);

#[async_trait]
impl Subrequest for RecordBody {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        let mut body = Vec::new();
        while let Some(bytes) = session.read_request_body().await.unwrap() {
            body.extend_from_slice(&bytes);
        }
        *self.0.lock().unwrap() = Some(body);
        session.respond_error(204).await.unwrap();
    }
}

/// Responds without reading its request body.
struct IgnoreBody;

#[async_trait]
impl Subrequest for IgnoreBody {
    async fn process_subrequest(
        self: Arc<Self>,
        mut session: Box<ServerSession>,
        _ctx: Box<SubrequestCtx>,
    ) {
        session.read_request().await.unwrap();
        session.respond_error(204).await.unwrap();
    }
}

/// A downstream module that uppercases request body chunks and counts its calls.
struct Uppercase(Arc<AtomicUsize>);

#[async_trait]
impl HttpModule for Uppercase {
    async fn request_body_filter(
        &mut self,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
    ) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if let Some(chunk) = body.as_mut() {
            *chunk = Bytes::from(chunk.to_ascii_uppercase());
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl HttpModuleBuilder for Uppercase {
    fn init(&self) -> Module {
        Box::new(Uppercase(self.0.clone()))
    }
}

fn uppercase_modules(calls: &Arc<AtomicUsize>) -> HttpModules {
    let mut modules = HttpModules::new();
    modules.add_module(Box::new(Uppercase(calls.clone())));
    modules
}

async fn buffered_session(buffered: Option<Bytes>, modules: &HttpModules) -> Session {
    let (mut client, server) = tokio::io::duplex(1024);
    client.write_all(POST).await.unwrap();
    tokio::spawn(async move {
        let mut response = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut client, &mut response).await;
    });
    let mut session =
        Session::new_h1_with_modules(Box::new(server) as pingora_core::protocols::Stream, modules);
    session.read_request().await.unwrap();
    while session.read_request_body().await.unwrap().is_some() {}
    session.set_buffered_body(buffered).unwrap();
    session
}

async fn pipe_to(
    mut session: Session,
    limit: usize,
    app: Arc<dyn Subrequest + Send + Sync>,
) -> SavedBody {
    let spawner = SubrequestSpawner::new(app);
    let ctx = SubrequestCtx::builder()
        .body_mode(BodyMode::ExpectBody)
        .build();
    let (subrequest, handle) = spawner.create_subrequest(session.as_downstream(), ctx);
    let state = tokio::time::timeout(
        Duration::from_secs(5),
        pipe_subrequest(
            &mut session,
            subrequest,
            handle,
            |task| Ok(Some(task)),
            InputBodyType::SaveBody(limit),
        ),
    )
    .await
    .expect("pipe hung")
    .unwrap_or_else(|e| panic!("pipe failed: {}", e.error));
    state.join_handle.unwrap().await.unwrap();
    state.saved_body.expect("SaveBody keeps a saved body")
}

async fn pipe_recorded(session: Session, limit: usize) -> (Option<Vec<u8>>, SavedBody) {
    let received = Arc::new(Mutex::new(None));
    let saved = pipe_to(session, limit, Arc::new(RecordBody(received.clone()))).await;
    let received = received.lock().unwrap().take();
    (received, saved)
}

const POST: &[u8] = b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 5\r\n\r\nhello";

#[tokio::test]
async fn buffered_body_is_forwarded_and_saved() {
    let session = buffered_session(Some(Bytes::from_static(b"hello")), &HttpModules::new()).await;
    let (received, saved) = pipe_recorded(session, 1024).await;

    assert_eq!(received.as_deref(), Some(&b"hello"[..]));
    assert!(saved.is_body_complete());
    assert_eq!(saved.body, vec![Bytes::from_static(b"hello")]);
}

#[tokio::test]
async fn buffered_body_over_limit_is_forwarded_but_saved_as_truncated() {
    let session = buffered_session(Some(Bytes::from_static(b"hello")), &HttpModules::new()).await;
    let (received, saved) = pipe_recorded(session, 4).await;

    assert_eq!(received.as_deref(), Some(&b"hello"[..]));
    assert!(!saved.is_body_complete());
}

/// Matches `SaveBody` for a request without a body: nothing is forwarded or saved.
#[tokio::test]
async fn filtered_empty_buffered_body_sends_no_body() {
    let session = buffered_session(None, &HttpModules::new()).await;
    let (received, saved) = pipe_recorded(session, 1024).await;

    assert_eq!(received.as_deref(), Some(&b""[..]));
    assert!(saved.body.is_empty());
}

#[tokio::test]
async fn buffered_body_runs_downstream_modules_before_forwarding_and_saving() {
    for handles_abandonment in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let modules = uppercase_modules(&calls);
        let mut session = buffered_session(Some(Bytes::from_static(b"hello")), &modules).await;
        session.set_handles_downstream_abandonment(handles_abandonment);
        let (received, saved) = pipe_recorded(session, 1024).await;

        assert_eq!(received.as_deref(), Some(&b"HELLO"[..]));
        assert!(saved.is_body_complete());
        assert_eq!(saved.body, vec![Bytes::from_static(b"HELLO")]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn buffered_body_is_not_sent_or_saved_when_subrequest_never_reads() {
    let calls = Arc::new(AtomicUsize::new(0));
    let modules = uppercase_modules(&calls);
    let session = buffered_session(Some(Bytes::from_static(b"hello")), &modules).await;
    let saved = pipe_to(session, 1024, Arc::new(IgnoreBody)).await;

    assert!(!saved.is_body_complete());
    assert!(saved.body.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
