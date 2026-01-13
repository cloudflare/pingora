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

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use http::HeaderMap;
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};

use crate::protocols::{http::HttpTask, l4::socket::SocketAddr, Digest};

use super::CustomMessageWrite;

#[doc(hidden)]
#[async_trait]
pub trait Session: Send + Sync + Unpin + 'static {
    fn req_header(&self) -> &RequestHeader;

    fn req_header_mut(&mut self) -> &mut RequestHeader;

    async fn read_body_bytes(&mut self) -> Result<Option<Bytes>>;

    async fn drain_request_body(&mut self) -> Result<()>;

    async fn write_response_header(&mut self, resp: Box<ResponseHeader>, end: bool) -> Result<()>;

    async fn write_response_header_ref(&mut self, resp: &ResponseHeader, end: bool) -> Result<()>;

    async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()>;

    async fn write_trailers(&mut self, trailers: HeaderMap) -> Result<()>;

    async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool>;

    fn set_read_timeout(&mut self, timeout: Option<Duration>);

    fn get_read_timeout(&self) -> Option<Duration>;

    fn set_write_timeout(&mut self, timeout: Option<Duration>);

    fn get_write_timeout(&self) -> Option<Duration>;

    fn set_total_drain_timeout(&mut self, timeout: Option<Duration>);

    fn get_total_drain_timeout(&self) -> Option<Duration>;

    fn request_summary(&self) -> String;

    fn response_written(&self) -> Option<&ResponseHeader>;

    async fn shutdown(&mut self, code: u32, ctx: &str);

    fn is_body_done(&mut self) -> bool;

    async fn finish(&mut self) -> Result<()>;

    fn is_body_empty(&mut self) -> bool;

    async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>>;

    fn body_bytes_sent(&self) -> usize;

    fn body_bytes_read(&self) -> usize;

    fn digest(&self) -> Option<&Digest>;

    fn digest_mut(&mut self) -> Option<&mut Digest>;

    fn client_addr(&self) -> Option<&SocketAddr>;

    fn server_addr(&self) -> Option<&SocketAddr>;

    fn pseudo_raw_h1_request_header(&self) -> Bytes;

    fn enable_retry_buffering(&mut self);

    fn retry_buffer_truncated(&self) -> bool;

    fn get_retry_buffer(&self) -> Option<Bytes>;

    async fn finish_custom(&mut self) -> Result<()>;

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>>;

    fn restore_custom_message_reader(
        &mut self,
        reader: Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>,
    ) -> Result<()>;

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>>;

    fn restore_custom_message_writer(&mut self, writer: Box<dyn CustomMessageWrite>) -> Result<()>;
}

#[doc(hidden)]
#[async_trait]
impl Session for () {
    fn req_header(&self) -> &RequestHeader {
        unreachable!("server session: req_header")
    }

    fn req_header_mut(&mut self) -> &mut RequestHeader {
        unreachable!("server session: req_header_mut")
    }

    async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        unreachable!("server session: read_body_bytes")
    }

    async fn drain_request_body(&mut self) -> Result<()> {
        unreachable!("server session: drain_request_body")
    }

    async fn write_response_header(
        &mut self,
        _resp: Box<ResponseHeader>,
        _end: bool,
    ) -> Result<()> {
        unreachable!("server session: write_response_header")
    }

    async fn write_response_header_ref(
        &mut self,
        _resp: &ResponseHeader,
        _end: bool,
    ) -> Result<()> {
        unreachable!("server session: write_response_header_ref")
    }

    async fn write_body(&mut self, _data: Bytes, _end: bool) -> Result<()> {
        unreachable!("server session: write_body")
    }

    async fn write_trailers(&mut self, _trailers: HeaderMap) -> Result<()> {
        unreachable!("server session: write_trailers")
    }

    async fn response_duplex_vec(&mut self, _tasks: Vec<HttpTask>) -> Result<bool> {
        unreachable!("server session: response_duplex_vec")
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_read_timeout")
    }

    fn get_read_timeout(&self) -> Option<Duration> {
        unreachable!("server_session: get_read_timeout")
    }

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_write_timeout")
    }

    fn get_write_timeout(&self) -> Option<Duration> {
        unreachable!("server_session: get_write_timeout")
    }

    fn set_total_drain_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("server session: set_total_drain_timeout")
    }

    fn get_total_drain_timeout(&self) -> Option<Duration> {
        unreachable!("server_session: get_total_drain_timeout")
    }

    fn request_summary(&self) -> String {
        unreachable!("server session: request_summary")
    }

    fn response_written(&self) -> Option<&ResponseHeader> {
        unreachable!("server session: response_written")
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        unreachable!("server session: shutdown")
    }

    fn is_body_done(&mut self) -> bool {
        unreachable!("server session: is_body_done")
    }

    async fn finish(&mut self) -> Result<()> {
        unreachable!("server session: finish")
    }

    fn is_body_empty(&mut self) -> bool {
        unreachable!("server session: is_body_empty")
    }

    async fn read_body_or_idle(&mut self, _no_body_expected: bool) -> Result<Option<Bytes>> {
        unreachable!("server session: read_body_or_idle")
    }

    fn body_bytes_sent(&self) -> usize {
        unreachable!("server session: body_bytes_sent")
    }

    fn body_bytes_read(&self) -> usize {
        unreachable!("server session: body_bytes_read")
    }

    fn digest(&self) -> Option<&Digest> {
        unreachable!("server session: digest")
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        unreachable!("server session: digest_mut")
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        unreachable!("server session: client_addr")
    }

    fn server_addr(&self) -> Option<&SocketAddr> {
        unreachable!("server session: server_addr")
    }

    fn pseudo_raw_h1_request_header(&self) -> Bytes {
        unreachable!("server session: pseudo_raw_h1_request_header")
    }

    fn enable_retry_buffering(&mut self) {
        unreachable!("server session: enable_retry_bufferings")
    }

    fn retry_buffer_truncated(&self) -> bool {
        unreachable!("server session: retry_buffer_truncated")
    }

    fn get_retry_buffer(&self) -> Option<Bytes> {
        unreachable!("server session: get_retry_buffer")
    }

    async fn finish_custom(&mut self) -> Result<()> {
        unreachable!("server session: finish_custom")
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        unreachable!("server session: get_custom_message_reader")
    }

    fn restore_custom_message_reader(
        &mut self,
        _reader: Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>,
    ) -> Result<()> {
        unreachable!("server session: get_custom_message_reader")
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        unreachable!("server session: get_custom_message_writer")
    }

    fn restore_custom_message_writer(
        &mut self,
        _writer: Box<dyn CustomMessageWrite>,
    ) -> Result<()> {
        unreachable!("server session: restore_custom_message_writer")
    }
}
