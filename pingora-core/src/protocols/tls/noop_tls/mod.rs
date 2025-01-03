// Copyright 2025 Cloudflare, Inc.
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

//! This is a set of stubs that provides the minimum types to let pingora work
//! without any tls providers configured

pub struct TlsRef;

pub type CaType = [CertWrapper];

#[derive(Debug)]
pub struct CertWrapper;

impl CertWrapper {
    pub fn not_after(&self) -> &str {
        ""
    }
}

pub mod connectors {
    use pingora_error::Result;

    use crate::{
        connectors::ConnectorOptions,
        protocols::{ALPN, IO},
        upstreams::peer::Peer,
    };

    use super::stream::SslStream;

    #[derive(Clone)]
    pub struct Connector {
        pub ctx: TlsConnector,
    }

    #[derive(Clone)]
    pub struct TlsConnector;

    pub struct TlsSettings;

    impl Connector {
        pub fn new(_: Option<ConnectorOptions>) -> Self {
            Self { ctx: TlsConnector }
        }
    }

    pub async fn connect<T, P>(
        _: T,
        _: &P,
        _: Option<ALPN>,
        _: &TlsConnector,
    ) -> Result<SslStream<T>>
    where
        T: IO,
        P: Peer + Send + Sync,
    {
        Ok(SslStream::default())
    }
}

pub mod listeners {
    use pingora_error::Result;
    use tokio::io::{AsyncRead, AsyncWrite};

    use super::stream::SslStream;

    pub struct Acceptor;

    pub struct TlsSettings;

    impl TlsSettings {
        pub fn build(&self) -> Acceptor {
            Acceptor
        }

        pub fn intermediate(_: &str, _: &str) -> Result<Self> {
            Ok(Self)
        }

        pub fn enable_h2(&mut self) {}
    }

    impl Acceptor {
        pub async fn tls_handshake<S: AsyncRead + AsyncWrite>(&self, _: S) -> Result<SslStream<S>> {
            unimplemented!("No tls feature was specified")
        }
    }
}

pub mod stream {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    use async_trait::async_trait;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use crate::protocols::{
        GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, Shutdown, Ssl, UniqueID,
    };

    /// A TLS session over a stream.
    #[derive(Debug)]
    pub struct SslStream<S> {
        marker: std::marker::PhantomData<S>,
    }

    impl<S> Default for SslStream<S> {
        fn default() -> Self {
            Self {
                marker: Default::default(),
            }
        }
    }

    impl<S> AsyncRead for SslStream<S>
    where
        S: AsyncRead + AsyncWrite,
    {
        fn poll_read(
            self: Pin<&mut Self>,
            _ctx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl<S> AsyncWrite for SslStream<S>
    where
        S: AsyncRead + AsyncWrite,
    {
        fn poll_write(
            self: Pin<&mut Self>,
            _ctx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _ctx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[async_trait]
    impl<S: Send> Shutdown for SslStream<S> {
        async fn shutdown(&mut self) {}
    }

    impl<S> UniqueID for SslStream<S> {
        fn id(&self) -> crate::protocols::UniqueIDType {
            0
        }
    }

    impl<S> Ssl for SslStream<S> {}

    impl<S> GetTimingDigest for SslStream<S> {
        fn get_timing_digest(&self) -> Vec<Option<crate::protocols::TimingDigest>> {
            vec![]
        }
    }

    impl<S> GetProxyDigest for SslStream<S> {
        fn get_proxy_digest(
            &self,
        ) -> Option<std::sync::Arc<crate::protocols::raw_connect::ProxyDigest>> {
            None
        }
    }

    impl<S> GetSocketDigest for SslStream<S> {
        fn get_socket_digest(&self) -> Option<std::sync::Arc<crate::protocols::SocketDigest>> {
            None
        }
    }

    impl<S> Peek for SslStream<S> {}
}

pub mod utils {
    use std::fmt::Display;

    use super::CertWrapper;

    #[derive(Debug, Clone, Hash)]
    pub struct CertKey;

    impl Display for CertKey {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            Ok(())
        }
    }

    pub fn get_organization_unit(_: &CertWrapper) -> Option<String> {
        None
    }
}
