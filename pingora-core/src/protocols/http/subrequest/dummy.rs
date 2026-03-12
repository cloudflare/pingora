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

use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, SocketDigest, Ssl, TimingDigest,
    UniqueID, UniqueIDType,
};
use async_trait::async_trait;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io::Cursor;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, Error, ReadBuf};

// An async IO stream that returns the request when being read from and dumps the data to the void
// when being write to
#[derive(Debug)]
pub(crate) struct DummyIO(Cursor<Vec<u8>>);

impl DummyIO {
    pub fn new(read_bytes: &[u8]) -> Self {
        DummyIO(Cursor::new(Vec::from(read_bytes)))
    }
}

impl AsyncRead for DummyIO {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), Error>> {
        if self.0.position() < self.0.get_ref().len() as u64 {
            Pin::new(&mut self.0).poll_read(cx, buf)
        } else {
            // all data is read, pending forever otherwise the stream is considered closed
            Poll::Pending
        }
    }
}

impl AsyncWrite for DummyIO {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
}

impl UniqueID for DummyIO {
    fn id(&self) -> UniqueIDType {
        0 // placeholder
    }
}

impl Ssl for DummyIO {}

impl GetTimingDigest for DummyIO {
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        vec![]
    }
}

impl GetProxyDigest for DummyIO {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        None
    }
}

impl GetSocketDigest for DummyIO {
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        None
    }
}

impl Peek for DummyIO {}

#[async_trait]
impl crate::protocols::Shutdown for DummyIO {
    async fn shutdown(&mut self) -> () {}
}

#[tokio::test]
async fn test_dummy_io() {
    use futures::FutureExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut dummy = DummyIO::new(&[1, 2]);
    let res = dummy.read_u8().await;
    assert_eq!(res.unwrap(), 1);
    let res = dummy.read_u8().await;
    assert_eq!(res.unwrap(), 2);
    let res = dummy.read_u8().now_or_never();
    assert!(res.is_none()); // pending forever
    let res = dummy.write_u8(0).await;
    assert!(res.is_ok());
}
