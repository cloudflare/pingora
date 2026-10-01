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

use super::*;
use pingora_core::apps::{HttpServerApp, HttpServerOptions};
use pingora_core::protocols::http::custom::CustomMessageWrite;
use pingora_core::protocols::http::ServerSession;
use pingora_proxy::{ProcessCustomSession, ProxyServiceBuilder};

#[path = "../utils/custom_downstream.rs"]
mod downstream;
use downstream::CustomDownstream;

#[derive(Default)]
pub(super) struct WriteFailure {
    started: Notify,
    released: Notify,
    finished: Notify,
    writes: AtomicUsize,
    errors: AtomicUsize,
}

impl WriteFailure {
    pub(super) fn release(&self) {
        self.released.notify_one();
    }
}

struct PendingWrite<'a>(&'a WriteFailure);

impl Drop for PendingWrite<'_> {
    fn drop(&mut self) {
        self.0.finished.notify_one();
    }
}

struct FailingCustomWriter(Arc<WriteFailure>);

#[async_trait]
impl CustomMessageWrite for FailingCustomWriter {
    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn write_custom_message(&mut self, message: Bytes) -> Result<()> {
        assert_eq!(message, Bytes::from_static(b"first"));
        assert_eq!(self.0.writes.fetch_add(1, Ordering::Relaxed), 0);
        let _pending = PendingWrite(&self.0);
        self.0.started.notify_one();
        self.0.released.notified().await;
        self.0.errors.fetch_add(1, Ordering::Relaxed);
        Error::e_explain(ErrorType::WriteError, "injected custom write failure")
    }

    async fn finish_custom(&mut self) -> Result<()> {
        Ok(())
    }
}

fn start_custom_proxy(
    observed: Arc<Observations>,
    handles_abandonment: bool,
    connector: StreamingConnector,
    failure: Arc<WriteFailure>,
) -> RunningProxy {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let on_custom: ProcessCustomSession<CacheFillProxy, StreamingConnector, CustomDownstream> =
        Arc::new(move |proxy, stream, shutdown| {
            let failure = failure.clone();
            let shutdown = shutdown.clone();
            Box::pin(async move {
                let mut http = ServerSession::new_http1(stream);
                assert!(http.read_request().await.unwrap());
                let downstream =
                    CustomDownstream::with_writer(http, Box::new(FailingCustomWriter(failure)));
                assert!(proxy
                    .process_new_http(ServerSession::new_custom(downstream), &shutdown)
                    .await
                    .is_none());
                None
            })
        });
    let mut options = HttpServerOptions::default();
    options.force_custom = true;
    let mut service = ProxyServiceBuilder::new(
        &Arc::new(ServerConf::default()),
        CacheFillProxy {
            cache: &*CACHE,
            observed,
            handles_abandonment,
        },
    )
    .custom(connector, on_custom)
    .server_options(options)
    .build();
    service.add_listener(
        ListenerConfig::tcp(address.to_string()).l4_buffer(L4BufferSettings::default().write(0)),
    );
    let fds: ListenFds = Arc::new(Fds::new().into());
    fds.lock().add(address.to_string(), listener.into_raw_fd());
    let (shutdown, watch) = watch::channel(false);
    let task = tokio::spawn(async move { service.start_service(Some(fds), watch, 1).await });
    RunningProxy {
        address,
        shutdown,
        task,
    }
}

#[derive(Clone, Copy)]
enum FailureTiming {
    OnAbandonment,
    WhileConnected,
}

/// Runs a cache fill whose first custom write toward the client stays pending until it fails:
/// `OnAbandonment` once the proxy has handled the client leaving, `WhileConnected` when the test
/// releases it.
async fn custom_write_failure(path: &str, timing: FailureTiming, handles_abandonment: bool) {
    assert!(read_cached_body(&CACHE, path).await.is_none());
    // Only abandonment handling cancels the write toward the client that left.
    let abandoned = matches!(timing, FailureTiming::OnAbandonment) && handles_abandonment;
    let failure = Arc::new(WriteFailure::default());
    let observed = Arc::new(Observations {
        expected_error: (!abandoned).then_some(ErrorType::WriteError),
        write_failure: Some(failure.clone()),
        ..Observations::default()
    });
    let upstream = CustomUpstream::new();
    let server = start_custom_proxy(
        observed.clone(),
        handles_abandonment,
        upstream.connector,
        failure.clone(),
    );
    let (release, released) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run(released));
    let mut client = send_request(server.address, path).await;
    within(
        "initial response was not delivered",
        read_initial_response(&mut client),
    )
    .await;
    within("custom write did not start", failure.started.notified()).await;
    assert_eq!(failure.writes.load(Ordering::Relaxed), 1);
    assert!(observed.initial_admission_active.load(Ordering::Relaxed));
    assert_eq!(
        observed.body_bytes.load(Ordering::Relaxed),
        FIRST_BODY.len()
    );

    let client = match timing {
        FailureTiming::OnAbandonment => {
            drop(client);
            within(
                "the proxy did not ignore the downstream error for the cache fill",
                observed.downstream_error_ignored.notified(),
            )
            .await;
            None
        }
        FailureTiming::WhileConnected => {
            failure.release();
            Some(client)
        }
    };
    within("custom write did not finish", failure.finished.notified()).await;
    assert_eq!(
        observed.callback_calls.load(Ordering::Relaxed),
        usize::from(abandoned)
    );

    if abandoned {
        // The write was canceled before its error, and the fill goes on without the client.
        assert!(observed.admitted_at_abandonment.load(Ordering::Relaxed));
        assert_eq!(failure.errors.load(Ordering::Relaxed), 0);
        assert!(!observed.completed.load(Ordering::Acquire));

        release.send(()).unwrap();
        assert!(within("custom producer did not finish", producer)
            .await
            .unwrap());
        within("cache fill did not complete", observed.finished.notified()).await;
        assert_eq!(observed.callback_calls.load(Ordering::Relaxed), 1);
        assert_eq!(observed.custom_messages.load(Ordering::Relaxed), 1);
        assert_eq!(
            read_cached_body(&CACHE, path)
                .await
                .expect("custom write cancellation discarded the cache fill"),
            [FIRST_BODY, LAST_BODY].concat()
        );
    } else {
        // The write error fails the request before the upstream sends the rest of the body.
        assert_eq!(failure.errors.load(Ordering::Relaxed), 1);
        within(
            "custom write error did not reach request logging",
            observed.finished.notified(),
        )
        .await;
        assert_eq!(observed.callback_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            observed.body_bytes.load(Ordering::Relaxed),
            FIRST_BODY.len()
        );
        producer.abort();
        assert!(producer.await.unwrap_err().is_cancelled());
    }
    drop(client);
    server.stop().await;
}

async fn custom_write_error_ready_at_abandonment(path: &str, handles_abandonment: bool) {
    // Independent requests expose the old selector's randomized choice between two ready branches.
    for attempt in 0..32 {
        custom_write_failure(
            &format!("{path}/{attempt}"),
            FailureTiming::OnAbandonment,
            handles_abandonment,
        )
        .await;
    }
}

#[tokio::test]
async fn custom_write_error_ready_at_abandonment_does_not_abort_cache_fill() {
    custom_write_error_ready_at_abandonment("/write-error-at-abandonment-gated", true).await;
    // Opted out, nothing cancels the pending write, so its error fails the request and the fill.
    custom_write_error_ready_at_abandonment("/write-error-at-abandonment-ungated", false).await;
}

#[tokio::test]
async fn custom_write_error_before_abandonment_propagates() {
    custom_write_failure(
        "/write-error-while-connected-gated",
        FailureTiming::WhileConnected,
        true,
    )
    .await;
    // Opted out, a connected client's failed custom write fails the request the same way.
    custom_write_failure(
        "/write-error-while-connected-ungated",
        FailureTiming::WhileConnected,
        false,
    )
    .await;
}
