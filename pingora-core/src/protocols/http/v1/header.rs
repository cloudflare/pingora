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

//! Cancel-safe header writing for HTTP/1.x

use bytes::Bytes;
use pingora_error::{Error, ErrorType::*, Result};
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::io::AsyncWrite;

use crate::protocols::l4::stream::async_write_vec::poll_write_all_buf;

#[allow(dead_code)]
enum HeaderWriteState {
    /// No write in progress
    Idle,
    /// Writing header bytes (original size, buffer)
    Writing(usize, Bytes),
    /// Flushing after write (original size to return)
    Flushing(usize),
    /// Write complete
    Done,
    /// Write timed out - cannot be reused
    TimedOut,
}

// Custom Debug implementation
impl std::fmt::Debug for HeaderWriteState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderWriteState::Idle => write!(f, "Idle"),
            HeaderWriteState::Writing(size, _) => write!(f, "Writing(size: {})", size),
            HeaderWriteState::Flushing(size) => write!(f, "Flushing(size: {})", size),
            HeaderWriteState::Done => write!(f, "Done"),
            HeaderWriteState::TimedOut => write!(f, "TimedOut"),
        }
    }
}

/// Internal state for the cancel-safe header write state machine.
///
/// Tracks the pending header bytes, write progress (idle → writing → flushing → done),
/// and an optional timeout that is lazily created on the first `Pending` poll.
#[allow(dead_code)]
struct SendHeaderState {
    /// Serialized header bytes ready to be written
    pending_header: Option<Bytes>,
    /// Whether to flush after writing
    should_flush: bool,
    /// Current write state
    write_state: HeaderWriteState,
    /// Timeout duration for this write task
    timeout_duration: Option<std::time::Duration>,
    /// Timeout future (only created if write returns Pending)
    timeout_fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>>,
}

// Custom Debug implementation since timeout_fut doesn't implement Debug
impl std::fmt::Debug for SendHeaderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendHeaderState")
            .field("pending_header", &self.pending_header)
            .field("should_flush", &self.should_flush)
            .field("write_state", &self.write_state)
            .field("timeout_duration", &self.timeout_duration)
            .field(
                "timeout_fut",
                &self.timeout_fut.as_ref().map(|_| "Some(Future)"),
            )
            .finish()
    }
}

impl SendHeaderState {
    #[allow(dead_code)]
    fn new() -> Self {
        SendHeaderState {
            pending_header: None,
            should_flush: false,
            write_state: HeaderWriteState::Idle,
            timeout_duration: None,
            timeout_fut: None,
        }
    }
}

/// Cancel-safe header writer for HTTP/1.x response headers.
///
/// This writer allows response headers to be written to a downstream connection
/// inside a `tokio::select!` loop without losing progress. If the write is
/// cancelled (e.g. because another branch of the select fires first), the
/// partially-written state is preserved and will be resumed on the next call to
/// [`write_current_header_task`](Self::write_current_header_task).
///
/// ## Usage
///
/// 1. Call [`send_header_task`](Self::send_header_task) with pre-serialized
///    header bytes, a flush flag, and an optional timeout.
/// 2. Await [`write_current_header_task`](Self::write_current_header_task)
///    (possibly inside `tokio::select!`).  The method returns `Ok(bytes_written)`
///    on success.
///
/// A timeout, if set, is enforced *across* cancellations — the clock keeps
/// ticking even when the future is dropped and re-polled.
#[allow(dead_code)]
pub struct HeaderWriter {
    // Boxed to reduce inline size. Only used by the cancel-safe proxy task API.
    send_header_state: Box<SendHeaderState>,
}

impl Default for HeaderWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl HeaderWriter {
    #[allow(dead_code)]
    pub fn new() -> Self {
        HeaderWriter {
            send_header_state: Box::new(SendHeaderState::new()),
        }
    }

    #[cfg(test)]
    pub fn has_pending_header_task(&self) -> bool {
        self.send_header_state.pending_header.is_some()
            || !matches!(
                self.send_header_state.write_state,
                HeaderWriteState::Idle | HeaderWriteState::Done | HeaderWriteState::TimedOut
            )
    }

    /// Queue serialized header bytes as a write task with an optional timeout.
    /// This is a non-async function that just saves the bytes.
    /// Call [`write_current_header_task`](Self::write_current_header_task) to actually perform the write.
    #[allow(dead_code)]
    pub fn send_header_task(
        &mut self,
        header_bytes: Bytes,
        should_flush: bool,
        timeout: Option<std::time::Duration>,
    ) {
        assert!(
            matches!(
                self.send_header_state.write_state,
                HeaderWriteState::Idle | HeaderWriteState::Done
            ),
            "send_header_task called while previous task is still in progress: {:?}",
            self.send_header_state.write_state
        );
        self.send_header_state.pending_header = Some(header_bytes);
        self.send_header_state.should_flush = should_flush;
        self.send_header_state.write_state = HeaderWriteState::Idle;
        self.send_header_state.timeout_duration = timeout;
        self.send_header_state.timeout_fut = None;
    }

    /// Async function that writes the current queued header task to the stream.
    /// This function is cancel-safe and can be called in a `tokio::select!` loop.
    /// Returns `Ok(bytes_written)` when complete, `Ok(0)` if no bytes to write.
    #[allow(dead_code)]
    pub async fn write_current_header_task<S>(&mut self, stream: &mut S) -> Result<usize>
    where
        S: AsyncWrite + Unpin + Send,
    {
        std::future::poll_fn(|cx| self.poll_write_current_header_task(cx, Pin::new(stream))).await
    }

    /// Poll-based implementation for writing the current header task.
    fn poll_write_current_header_task<S>(
        &mut self,
        cx: &mut Context<'_>,
        stream: Pin<&mut S>,
    ) -> Poll<Result<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Check if already timed out - don't allow reuse
        if matches!(
            self.send_header_state.write_state,
            HeaderWriteState::TimedOut
        ) {
            return Poll::Ready(Error::e_explain(
                WriteTimedout,
                "header write task previously timed out",
            ));
        }

        // First, try the write operation
        match self.poll_do_write_header_and_flush(cx, stream) {
            Poll::Ready(Ok(size)) => {
                // Write completed! Clear timeout and return
                if matches!(self.send_header_state.write_state, HeaderWriteState::Done) {
                    self.send_header_state.timeout_fut = None;
                }
                return Poll::Ready(Ok(size));
            }
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => {
                // Write is pending - now check timeout
            }
        }

        // Lazy timeout optimization: Polls write first, creates timeout only if needed.
        // This follows the pattern from `pingora_timeout::Timeout` to avoid allocating
        // timeout futures when writes complete immediately (the common case).
        if let Some(duration) = self.send_header_state.timeout_duration {
            let timeout = self.send_header_state.timeout_fut.get_or_insert_with(|| {
                Box::pin(pingora_timeout::sleep(duration))
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>
            });

            if timeout.as_mut().poll(cx).is_ready() {
                // Timeout fired!
                self.send_header_state.write_state = HeaderWriteState::TimedOut;
                self.send_header_state.timeout_fut = None;
                return Poll::Ready(Error::e_explain(
                    WriteTimedout,
                    "writing header task timed out",
                ));
            }
        }

        // Both write and timeout are pending
        Poll::Pending
    }

    /// Poll-based helper to write header bytes and optionally flush.
    /// Handles state transitions explicitly.
    fn poll_do_write_header_and_flush<S>(
        &mut self,
        cx: &mut Context<'_>,
        mut stream: Pin<&mut S>,
    ) -> Poll<Result<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Handle Idle state - take pending header and transition to Writing
        if matches!(self.send_header_state.write_state, HeaderWriteState::Idle) {
            if let Some(header_bytes) = self.send_header_state.pending_header.take() {
                let size = header_bytes.len();
                self.send_header_state.write_state = HeaderWriteState::Writing(size, header_bytes);
            } else {
                // No pending header
                self.send_header_state.write_state = HeaderWriteState::Done;
                return Poll::Ready(Ok(0));
            }
        }

        // Write if in Writing state
        if let HeaderWriteState::Writing(original_size, ref mut buf) =
            self.send_header_state.write_state
        {
            let size = original_size;
            ready!(poll_write_all_buf(cx, stream.as_mut(), buf))
                .map_err(|e| Error::because(WriteError, "writing response header", e))?;

            // Write complete - transition to next state
            if self.send_header_state.should_flush {
                self.send_header_state.write_state = HeaderWriteState::Flushing(size);
            } else {
                self.send_header_state.write_state = HeaderWriteState::Done;
                return Poll::Ready(Ok(size));
            }
        }

        // Handle the state after writing (or if we started in a non-Writing state)
        match self.send_header_state.write_state {
            HeaderWriteState::Flushing(size) => {
                ready!(stream.as_mut().poll_flush(cx))
                    .map_err(|e| Error::because(WriteError, "flushing response header", e))?;
                // Flush complete - transition to Done
                self.send_header_state.write_state = HeaderWriteState::Done;
                Poll::Ready(Ok(size))
            }
            HeaderWriteState::Done => Poll::Ready(Ok(0)),
            HeaderWriteState::TimedOut => Poll::Ready(Error::e_explain(
                WriteTimedout,
                "header write task previously timed out",
            )),
            HeaderWriteState::Idle => {
                unreachable!("Idle state should have been handled above")
            }
            HeaderWriteState::Writing(..) => {
                unreachable!("Writing state should have been handled above")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io::Builder;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn test_simple_header_write() {
        init_log();
        let header_data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";

        let mut mock_io = Builder::new().write(header_data).build();

        let mut header_writer = HeaderWriter::new();
        header_writer.send_header_task(Bytes::from_static(header_data), false, None);

        let result = header_writer.write_current_header_task(&mut mock_io).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), header_data.len());
    }

    #[tokio::test]
    async fn test_header_write_with_flush() {
        init_log();
        let header_data = b"HTTP/1.1 200 OK\r\n\r\n";

        let mut mock_io = Builder::new().write(header_data).build();

        let mut header_writer = HeaderWriter::new();
        header_writer.send_header_task(Bytes::from_static(header_data), true, None);

        let result = header_writer.write_current_header_task(&mut mock_io).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), header_data.len());
    }

    // Uses start_paused for deterministic timer-based cancellation in select!
    #[tokio::test(start_paused = true)]
    async fn test_cancel_safe_header_write() {
        init_log();
        let header_data = b"HTTP/1.1 200 OK\r\nServer: pingora\r\n\r\n";

        // Mock that blocks to allow cancellation
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(100))
            .write(header_data)
            .build();

        let mut header_writer = HeaderWriter::new();
        header_writer.send_header_task(Bytes::from_static(header_data), false, None);

        let mut cancel_count = 0;

        loop {
            if !header_writer.has_pending_header_task() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    cancel_count += 1;
                }
                result = header_writer.write_current_header_task(&mut mock_io) => {
                    assert!(result.is_ok());
                    assert_eq!(result.unwrap(), header_data.len());
                    break;
                }
            }
        }

        assert!(cancel_count > 0, "Should have cancelled at least once");
    }

    #[tokio::test]
    async fn test_header_write_timeout() {
        init_log();
        let header_data = b"HTTP/1.1 200 OK\r\n\r\n";

        // Mock that blocks forever
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_secs(1000))
            .build();

        let mut header_writer = HeaderWriter::new();
        header_writer.send_header_task(
            Bytes::from_static(header_data),
            false,
            Some(std::time::Duration::from_millis(50)),
        );

        let result = header_writer.write_current_header_task(&mut mock_io).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().etype(), &WriteTimedout);
    }

    #[tokio::test]
    async fn test_header_write_timeout_persists() {
        init_log();
        let header_data = b"HTTP/1.1 200 OK\r\n\r\n";

        // Mock that blocks for a while
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(200))
            .build();

        let mut header_writer = HeaderWriter::new();
        header_writer.send_header_task(
            Bytes::from_static(header_data),
            false,
            Some(std::time::Duration::from_millis(100)),
        );

        let mut attempts = 0;
        let mut timedout = false;

        loop {
            if !header_writer.has_pending_header_task() {
                break;
            }

            attempts += 1;

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    continue;
                }
                result = header_writer.write_current_header_task(&mut mock_io) => {
                    match result {
                        Ok(_) => break,
                        Err(e) if e.etype() == &WriteTimedout => {
                            timedout = true;
                            break;
                        }
                        Err(e) => panic!("Unexpected error: {:?}", e),
                    }
                }
            }
        }

        assert!(timedout, "Timeout should have fired");
        assert!(attempts >= 5, "Should have had multiple attempts");
    }
}
