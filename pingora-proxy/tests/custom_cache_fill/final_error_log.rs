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

//! The final "Fail to proxy" error log reports the status an application's `fail_to_proxy`
//! returned, whether or not the failed request's downstream abandoned its cache fill.

use super::*;
use pingora_proxy::FailToProxy;
use std::sync::{LazyLock, Mutex, Once};

/// Nothing is admitted into it: each fill fails before its response is complete.
static FAILED_FILL_CACHE: LazyLock<MemCache> = LazyLock::new(MemCache::new);

/// A status no default mapping produces, so only the override can be its source.
const OVERRIDE_STATUS: u16 = 599;

/// Error-level log records of this test binary. Nothing else in it installs a logger.
struct CapturedErrorLogs(Mutex<Vec<String>>);

static CAPTURED_ERROR_LOGS: CapturedErrorLogs = CapturedErrorLogs(Mutex::new(Vec::new()));

impl log::Log for CapturedErrorLogs {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Error
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

fn capture_error_logs() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&CAPTURED_ERROR_LOGS).expect("another logger is installed");
        log::set_max_level(log::LevelFilter::Error);
    });
}

/// The final "Fail to proxy" error logs of requests for `path`. Tests in this binary run
/// concurrently, so the request summary at the end of each line tells them apart.
fn final_error_logs(path: &str) -> Vec<String> {
    CAPTURED_ERROR_LOGS
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.starts_with("Fail to proxy: ") && line.contains(path))
        .cloned()
        .collect()
}

/// [`CacheFillProxy`] with an application `fail_to_proxy` that reports [`OVERRIDE_STATUS`].
struct FailToProxyOverride(CacheFillProxy);

#[async_trait]
impl ProxyHttp for FailToProxyOverride {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    fn should_handle_downstream_abandonment(&self, session: &Session, ctx: &()) -> bool {
        self.0.should_handle_downstream_abandonment(session, ctx)
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut ()) -> Result<Box<HttpPeer>> {
        self.0.upstream_peer(session, ctx).await
    }

    fn request_cache_filter(&self, session: &mut Session, ctx: &mut ()) -> Result<()> {
        self.0.request_cache_filter(session, ctx)
    }

    fn cache_key_callback(&self, session: &Session, ctx: &mut ()) -> Result<CacheKey> {
        self.0.cache_key_callback(session, ctx)
    }

    fn response_cache_filter(
        &self,
        session: &Session,
        response: &ResponseHeader,
        ctx: &mut (),
    ) -> Result<RespCacheable> {
        self.0.response_cache_filter(session, response, ctx)
    }

    fn downstream_abandoned_during_cache_fill(&self, session: &Session, ctx: &mut ()) {
        self.0.downstream_abandoned_during_cache_fill(session, ctx)
    }

    fn suppress_proxy_warn_log(
        &self,
        session: &Session,
        ctx: &(),
        error: &Error,
        context: ProxyWarnLogContext,
    ) -> bool {
        self.0.suppress_proxy_warn_log(session, ctx, error, context)
    }

    async fn fail_to_proxy(
        &self,
        _session: &mut Session,
        _e: &Error,
        _ctx: &mut (),
    ) -> FailToProxy {
        FailToProxy {
            error_code: OVERRIDE_STATUS,
            can_reuse_downstream: false,
        }
    }

    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut ()) {
        self.0.logging(session, error, ctx).await
    }
}

/// The client leaves after the first body chunk, then the upstream fails.
async fn assert_final_error_log_reports_override_status(path: &'static str, opted_in: bool) {
    capture_error_logs();
    let observed = Arc::new(Observations {
        expected_error: Some(ErrorType::ReadError),
        ..Observations::default()
    });
    let upstream = CustomUpstream::failing_when_producer_drops();
    let server = RunningProxy::start(
        FailToProxyOverride(CacheFillProxy {
            cache: &*FAILED_FILL_CACHE,
            observed: Arc::clone(&observed),
            handles_abandonment: opted_in,
        }),
        upstream.connector,
    );
    let (fail, failed) = oneshot::channel();
    let producer = tokio::spawn(upstream.producer.run_until_failure(failed));
    let mut client = within(
        "HTTP client did not connect",
        TcpStream::connect(server.address),
    )
    .await
    .unwrap();
    within(
        "HTTP request was not written",
        client.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
    )
    .await
    .unwrap();
    let mut client = BufReader::new(client);
    within(
        "initial response was not delivered",
        read_initial_response(&mut client),
    )
    .await;

    drop(client);
    if opted_in {
        within(
            "downstream abandonment was not reported",
            observed.abandoned.notified(),
        )
        .await;
    } else {
        within(
            "the proxy did not ignore the downstream error for the cache fill",
            observed.downstream_error_ignored.notified(),
        )
        .await;
    }
    fail.send(()).unwrap();
    within("upstream producer did not stop", producer)
        .await
        .unwrap();
    // The final error log precedes `logging`, which notifies this.
    within(
        "failed request did not finish",
        observed.finished.notified(),
    )
    .await;
    assert_eq!(
        observed.callback_calls.load(Ordering::Relaxed),
        usize::from(opted_in)
    );

    let logs = final_error_logs(path);
    assert_eq!(logs.len(), 1, "expected one final error log: {logs:?}");
    assert!(
        logs[0].contains(&format!(", status: {OVERRIDE_STATUS}, ")),
        "the final error log does not report the application's status: {}",
        logs[0]
    );
    server.stop().await;
}

#[tokio::test]
async fn final_error_log_of_abandoned_request_reports_fail_to_proxy_override() {
    assert_final_error_log_reports_override_status("/custom-final-error-log-abandoned", true).await;
}

#[tokio::test]
async fn final_error_log_reports_fail_to_proxy_override_without_the_opt_in() {
    assert_final_error_log_reports_override_status("/custom-final-error-log-opted-out", false)
        .await;
}
