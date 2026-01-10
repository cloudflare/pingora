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

#[async_trait]
pub trait BodyWrite: Send + Sync + Unpin + 'static {
    async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()>;
    async fn finish(&mut self) -> Result<()>;
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
