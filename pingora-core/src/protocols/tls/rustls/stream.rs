// Copyright 2024 Cloudflare, Inc.
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

use async_trait::async_trait;
use pingora_error::ErrorType::{AcceptError, ConnectError, TLSHandshakeFailure};
use pingora_error::{Error, ImmutStr, OrErr, Result};
use pingora_rustls::NoDebug;
use pingora_rustls::TlsAcceptor as RusTlsAcceptor;
use pingora_rustls::TlsStream as RusTlsStream;
use pingora_rustls::{Accept, Connect, ServerName, TlsConnector};
use std::fmt::Debug;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::listeners::tls::Acceptor;
use crate::listeners::ALPN;
use crate::protocols::digest::{GetSocketDigest, SocketDigest, TimingDigest};
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::tls::InnerTlsStream;
use crate::protocols::tls::SslDigest;
use crate::protocols::{GetProxyDigest, GetTimingDigest};

#[derive(Debug)]
pub struct InnerStream<T> {
    pub(crate) stream: Option<RusTlsStream<T>>,
    connect: NoDebug<Option<Connect<T>>>,
    accept: NoDebug<Option<Accept<T>>>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> InnerStream<T> {
    /// Create a new TLS connection from the given `stream`
    ///
    /// The caller needs to perform [`Self::connect()`] or [`Self::accept()`] to perform TLS
    /// handshake after.
    pub(crate) async fn from_connector(
        connector: &TlsConnector,
        server: ServerName<'_>,
        stream: T,
    ) -> Result<Self> {
        let connect = connector.connect(server.to_owned(), stream);
        Ok(InnerStream {
            accept: None.into(),
            connect: Some(connect).into(),
            stream: None,
        })
    }

    pub(crate) async fn from_acceptor(acceptor: &Acceptor, stream: T) -> Result<Self> {
        let tls_acceptor = acceptor.inner().downcast_ref::<RusTlsAcceptor>().unwrap();
        let accept = tls_acceptor.accept(stream);

        Ok(InnerStream {
            accept: Some(accept).into(),
            connect: None.into(),
            stream: None,
        })
    }
}

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send> InnerTlsStream for InnerStream<T> {
    /// Connect to the remote TLS server as a client
    async fn connect(&mut self) -> Result<()> {
        let connect = &mut (*self.connect);

        if let Some(ref mut connect) = connect {
            let stream = connect
                .await
                .explain_err(TLSHandshakeFailure, |e| format!("tls connect error: {e}"))?;
            self.stream = Some(RusTlsStream::Client(stream));
            self.connect = None.into();

            Ok(())
        } else {
            Err(Error::explain(
                ConnectError,
                ImmutStr::from("TLS connect not available to perform handshake."),
            ))
        }
    }

    /// Finish the TLS handshake from client as a server
    /// no-op implementation within Rustls, handshake is performed during creation of stream.
    async fn accept(&mut self) -> Result<()> {
        let accept = &mut (*self.accept);

        if let Some(ref mut accept) = accept {
            let stream = accept
                .await
                .explain_err(TLSHandshakeFailure, |e| format!("tls connect error: {e}"))?;
            self.stream = Some(RusTlsStream::Server(stream));
            self.connect = None.into();

            Ok(())
        } else {
            Err(Error::explain(
                AcceptError,
                ImmutStr::from("TLS accept not available to perform handshake."),
            ))
        }
    }

    fn digest(&mut self) -> Option<Arc<SslDigest>> {
        Some(Arc::new(SslDigest::from_stream(&self.stream)))
    }

    fn selected_alpn_proto(&mut self) -> Option<ALPN> {
        if let Some(stream) = self.stream.as_ref() {
            let proto = stream.get_ref().1.alpn_protocol();
            match proto {
                None => None,
                Some(raw) => ALPN::from_wire_selected(raw),
            }
        } else {
            None
        }
    }
}

impl<S> GetSocketDigest for InnerStream<S>
where
    S: GetSocketDigest,
{
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        if let Some(stream) = self.stream.as_ref() {
            stream.get_ref().0.get_socket_digest()
        } else {
            None
        }
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.stream
            .as_mut()
            .unwrap()
            .get_mut()
            .0
            .set_socket_digest(socket_digest)
    }
}

impl<S> GetTimingDigest for InnerStream<S>
where
    S: GetTimingDigest,
{
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        self.stream
            .as_ref()
            .unwrap()
            .get_ref()
            .0
            .get_timing_digest()
    }
}

impl<S> GetProxyDigest for InnerStream<S>
where
    S: GetProxyDigest,
{
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        if let Some(stream) = self.stream.as_ref() {
            stream.get_ref().0.get_proxy_digest()
        } else {
            None
        }
    }
}
