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
use log::debug;
use pingora_error::Result;
use tokio_stream::StreamExt;

pub mod client;
pub mod server;

pub const CUSTOM_MESSAGE_QUEUE_SIZE: usize = 128;

pub fn is_informational_except_101<T: PartialOrd<u32>>(code: T) -> bool {
    // excluding `101 Switching Protocols`, because it's not followed by any other
    // response and it's a final
    // The WebSocket Protocol https://datatracker.ietf.org/doc/html/rfc6455
    code > 99 && code < 200 && code != 101
}

#[async_trait]
pub trait CustomMessageWrite: Send + Sync + Unpin + 'static {
    fn set_write_timeout(&mut self, timeout: Option<Duration>);
    async fn write_custom_message(&mut self, msg: Bytes) -> Result<()>;
    async fn finish_custom(&mut self) -> Result<()>;
}

#[doc(hidden)]
#[async_trait]
impl CustomMessageWrite for () {
    fn set_write_timeout(&mut self, _timeout: Option<Duration>) {}

    async fn write_custom_message(&mut self, msg: Bytes) -> Result<()> {
        debug!("write_custom_message: {:?}", msg);
        Ok(())
    }

    async fn finish_custom(&mut self) -> Result<()> {
        debug!("finish_custom");
        Ok(())
    }
}

/// Writes a request body to a custom upstream.
///
/// The proxy takes the writer from the client session once the request header is written, and
/// sends the request body through it:
///
/// - [`write_all_buf`](Self::write_all_buf) is called with each piece of the body as it arrives,
///   except empty pieces before the end, and should write all of it.
/// - [`finish`](Self::finish) is called when the body ends.
/// - [`upgrade_body_writer`](Self::upgrade_body_writer) is called each time the proxy handles a
///   piece of the body once the downstream session is upgraded, so it can be called more than
///   once.
/// - [`cleanup`](Self::cleanup) is called when the proxy finishes relaying the response without
///   an error. It does not mean that the upload completed: the body may not have ended, for
///   example when the client left while its response was being cached.
///
/// Once `write_all_buf` or `finish` fails, the proxy sends no more of the body and drops the
/// writer without calling `cleanup`, even when the upstream still sends a complete response and
/// the attempt succeeds.
#[async_trait]
pub trait BodyWrite: Send + Sync + Unpin + 'static {
    async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()>;
    async fn finish(&mut self) -> Result<()>;
    async fn cleanup(&mut self) -> Result<()>;
    fn upgrade_body_writer(&mut self);
}

pub async fn drain_custom_messages(
    reader: Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>>,
) -> Result<()> {
    let Some(mut reader) = reader else {
        return Ok(());
    };

    while let Some(res) = reader.next().await {
        let msg = res?;
        debug!("consume_custom_messages: {msg:?}");
    }

    Ok(())
}

#[macro_export]
macro_rules! custom_session {
    ($base_obj:ident . $($method_tokens:tt)+) => {
        if let Some(custom_session) = $base_obj.as_custom_mut() {
            #[allow(clippy::semicolon_if_nothing_returned)]
            custom_session.$($method_tokens)+;
        }
    };
}
