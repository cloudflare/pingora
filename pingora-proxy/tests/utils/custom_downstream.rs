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

//! A custom downstream session that carries HTTP/1 over a real connection, reports its response
//! complete when that HTTP/1 session does, and hands its custom output to an injected writer.

// Each test binary that includes this file uses only part of it.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::Stream;
use http::HeaderMap;
use pingora_core::protocols::http::custom::{server::Session, CustomMessageWrite};
use pingora_core::protocols::http::{HttpTask, ServerSession};
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_core::protocols::Digest;
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};

type CustomReader = Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>;

/// The custom messages written to a custom session, such as a [`CustomDownstream`], and how
/// often its custom output was finished.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CustomOutput {
    pub messages: Vec<Bytes>,
    pub finishes: usize,
}

/// Records the custom messages written to it, and how often it was finished, in a
/// [`CustomOutput`].
pub struct RecordingCustomWriter(pub Arc<Mutex<CustomOutput>>);

#[async_trait]
impl CustomMessageWrite for RecordingCustomWriter {
    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn write_custom_message(&mut self, message: Bytes) -> Result<()> {
        let mut output = self.0.lock().unwrap();
        assert_eq!(output.finishes, 0, "custom message written after finish");
        output.messages.push(message);
        Ok(())
    }

    async fn finish_custom(&mut self) -> Result<()> {
        self.0.lock().unwrap().finishes += 1;
        Ok(())
    }
}

/// Carries HTTP over a real connection and delegates custom output to its writer. It has no
/// custom input.
pub struct CustomDownstream {
    http: ServerSession<()>,
    request_body: BytesMut,
    reader: Option<CustomReader>,
    writer: Option<Box<dyn CustomMessageWrite>>,
}

impl CustomDownstream {
    /// Records custom output in `output`.
    pub fn new(http: ServerSession<()>, output: Arc<Mutex<CustomOutput>>) -> Self {
        Self::with_writer(http, Box::new(RecordingCustomWriter(output)))
    }

    pub fn with_writer(http: ServerSession<()>, writer: Box<dyn CustomMessageWrite>) -> Self {
        Self {
            http,
            request_body: BytesMut::new(),
            reader: Some(Box::new(futures::stream::empty())),
            writer: Some(writer),
        }
    }
}

impl Session for CustomDownstream {
    fn req_header(&self) -> &RequestHeader {
        self.http.req_header()
    }

    fn req_header_mut(&mut self) -> &mut RequestHeader {
        self.http.req_header_mut()
    }

    async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        self.http.read_request_body().await
    }

    async fn drain_request_body(&mut self) -> Result<()> {
        self.http.drain_request_body().await
    }

    async fn write_response_header(
        &mut self,
        response: Box<ResponseHeader>,
        end: bool,
    ) -> Result<()> {
        self.http.write_response_header(response).await?;
        if end {
            self.http.finish_body().await?;
        }
        Ok(())
    }

    async fn write_response_header_ref(
        &mut self,
        response: &ResponseHeader,
        end: bool,
    ) -> Result<()> {
        self.http.write_response_header_ref(response).await?;
        if end {
            self.http.finish_body().await?;
        }
        Ok(())
    }

    async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        self.http.write_response_body(data, end).await
    }

    async fn write_trailers(&mut self, trailers: HeaderMap) -> Result<()> {
        self.http.write_response_trailers(trailers).await
    }

    async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool> {
        self.http.response_duplex_vec(tasks).await
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.http.set_read_timeout(timeout);
    }

    fn get_read_timeout(&self) -> Option<Duration> {
        self.http.get_read_timeout()
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.http.set_write_timeout(timeout);
    }

    fn get_write_timeout(&self) -> Option<Duration> {
        self.http.get_write_timeout()
    }

    fn set_total_drain_timeout(&mut self, timeout: Option<Duration>) {
        self.http.set_total_drain_timeout(timeout);
    }

    fn get_total_drain_timeout(&self) -> Option<Duration> {
        self.http.get_total_drain_timeout()
    }

    fn request_summary(&self) -> String {
        self.http.request_summary()
    }

    fn response_written(&self) -> Option<&ResponseHeader> {
        self.http.response_written()
    }

    fn is_response_complete(&self) -> bool {
        self.http.is_response_complete()
    }

    async fn shutdown(&mut self, _code: u32, _context: &str) {
        self.http.shutdown().await;
    }

    fn is_body_done(&mut self) -> bool {
        self.http.is_body_done()
    }

    async fn finish(&mut self) -> Result<()> {
        self.http.finish_body().await
    }

    fn is_body_empty(&mut self) -> bool {
        self.http.is_body_empty()
    }

    async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if no_body_expected {
            return self.http.read_body_or_idle(true).await;
        }

        // Preserve partial reads across select cancellation and forward one oversized chunk.
        while let Some(chunk) = self.http.read_request_body().await? {
            self.request_body.extend_from_slice(&chunk);
        }
        Ok(Some(self.request_body.split().freeze()))
    }

    fn body_bytes_sent(&self) -> usize {
        self.http.body_bytes_sent()
    }

    fn body_bytes_read(&self) -> usize {
        self.http.body_bytes_read()
    }

    fn digest(&self) -> Option<&Digest> {
        self.http.digest()
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        self.http.digest_mut()
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        self.http.client_addr()
    }

    fn server_addr(&self) -> Option<&SocketAddr> {
        self.http.server_addr()
    }

    fn pseudo_raw_h1_request_header(&self) -> Bytes {
        self.http.to_h1_raw()
    }

    fn enable_retry_buffering(&mut self) {
        self.http.enable_retry_buffering();
    }

    fn retry_buffer_truncated(&self) -> bool {
        self.http.retry_buffer_truncated()
    }

    fn get_retry_buffer(&self) -> Option<Bytes> {
        self.http.get_retry_buffer()
    }

    async fn finish_custom(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.finish_custom().await?;
        }
        Ok(())
    }

    fn take_custom_message_reader(&mut self) -> Option<CustomReader> {
        self.reader.take()
    }

    fn restore_custom_message_reader(&mut self, reader: CustomReader) -> Result<()> {
        assert!(self.reader.replace(reader).is_none());
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        self.writer.take()
    }

    fn restore_custom_message_writer(&mut self, writer: Box<dyn CustomMessageWrite>) -> Result<()> {
        assert!(self.writer.replace(writer).is_none());
        Ok(())
    }
}
