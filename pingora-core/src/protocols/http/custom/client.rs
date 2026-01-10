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

use crate::protocols::{l4::socket::SocketAddr, Digest, UniqueIDType};

use super::{BodyWrite, CustomMessageWrite};

#[doc(hidden)]
#[async_trait]
pub trait Session: Send + Sync + Unpin + 'static {
    async fn write_request_header(&mut self, req: Box<RequestHeader>, end: bool) -> Result<()>;

    async fn write_request_body(&mut self, data: Bytes, end: bool) -> Result<()>;

    async fn finish_request_body(&mut self) -> Result<()>;

    fn set_read_timeout(&mut self, timeout: Option<Duration>);

    fn set_write_timeout(&mut self, timeout: Option<Duration>);

    async fn read_response_header(&mut self) -> Result<()>;

    async fn read_response_body(&mut self) -> Result<Option<Bytes>>;

    fn response_finished(&self) -> bool;

    async fn shutdown(&mut self, code: u32, ctx: &str);

    fn response_header(&self) -> Option<&ResponseHeader>;

    fn digest(&self) -> Option<&Digest>;

    fn digest_mut(&mut self) -> Option<&mut Digest>;

    fn server_addr(&self) -> Option<&SocketAddr>;

    fn client_addr(&self) -> Option<&SocketAddr>;

    async fn read_trailers(&mut self) -> Result<Option<HeaderMap>>;

    fn fd(&self) -> UniqueIDType;

    async fn check_response_end_or_error(&mut self, headers: bool) -> Result<bool>;

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>>;

    async fn finish_custom(&mut self) -> Result<()>;

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>>;

    async fn drain_custom_messages(&mut self) -> Result<()>;

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>>;
}

#[doc(hidden)]
#[async_trait]
impl Session for () {
    async fn write_request_header(&mut self, _req: Box<RequestHeader>, _end: bool) -> Result<()> {
        unreachable!("client session: write_request_header")
    }

    async fn write_request_body(&mut self, _data: Bytes, _end: bool) -> Result<()> {
        unreachable!("client session: write_request_body")
    }

    async fn finish_request_body(&mut self) -> Result<()> {
        unreachable!("client session: finish_request_body")
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("client session: set_read_timeout")
    }

    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {
        unreachable!("client session: set_write_timeout")
    }

    async fn read_response_header(&mut self) -> Result<()> {
        unreachable!("client session: read_response_header")
    }

    async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        unreachable!("client session: read_response_body")
    }

    fn response_finished(&self) -> bool {
        unreachable!("client session: response_finished")
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        unreachable!("client session: shutdown")
    }

    fn response_header(&self) -> Option<&ResponseHeader> {
        unreachable!("client session: response_header")
    }

    fn digest(&self) -> Option<&Digest> {
        unreachable!("client session: digest")
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        unreachable!("client session: digest_mut")
    }

    fn server_addr(&self) -> Option<&SocketAddr> {
        unreachable!("client session: server_addr")
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        unreachable!("client session: client_addr")
    }

    async fn finish_custom(&mut self) -> Result<()> {
        unreachable!("client session: finish_custom")
    }

    async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        unreachable!("client session: read_trailers")
    }

    fn fd(&self) -> UniqueIDType {
        unreachable!("client session: fd")
    }

    async fn check_response_end_or_error(&mut self, _headers: bool) -> Result<bool> {
        unreachable!("client session: check_response_end_or_error")
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        unreachable!("client session: get_custom_message_reader")
    }

    async fn drain_custom_messages(&mut self) -> Result<()> {
        unreachable!("client session: drain_custom_messages")
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        unreachable!("client session: get_custom_message_writer")
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        unreachable!("client session: take_request_body_writer")
    }
}
