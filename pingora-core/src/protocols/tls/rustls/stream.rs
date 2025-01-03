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

use std::io::Result as IoResult;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use crate::listeners::tls::Acceptor;
use crate::protocols::raw_connect::ProxyDigest;
use crate::protocols::{tls::SslDigest, Peek, TimingDigest, UniqueIDType};
use crate::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, SocketDigest, Ssl, UniqueID, ALPN,
};
use crate::utils::tls::get_organization_serial_bytes;
use pingora_error::ErrorType::{AcceptError, ConnectError, InternalError, TLSHandshakeFailure};
use pingora_error::{OkOrErr, OrErr, Result};
use pingora_rustls::TlsStream as RusTlsStream;
use pingora_rustls::{hash_certificate, NoDebug};
use pingora_rustls::{Accept, Connect, ServerName, TlsConnector};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use x509_parser::nom::AsBytes;

#[derive(Debug)]
pub struct InnerStream<T> {
    pub(crate) stream: Option<RusTlsStream<T>>,
    connect: NoDebug<Option<Connect<T>>>,
    accept: NoDebug<Option<Accept<T>>>,
}

/// The TLS connection
#[derive(Debug)]
pub struct TlsStream<T> {
    tls: InnerStream<T>,
    digest: Option<Arc<SslDigest>>,
    timing: TimingDigest,
}

impl<T> TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Create a new TLS connection from the given `stream`
    ///
    /// Using RustTLS the stream is only returned after the handshake.
    /// The caller does therefor not need to perform [`Self::connect()`].
    pub async fn from_connector(connector: &TlsConnector, domain: &str, stream: T) -> Result<Self> {
        let server = ServerName::try_from(domain).or_err_with(InternalError, || {
            format!("Invalid Input: Failed to parse domain: {domain}")
        })?;

        let tls = InnerStream::from_connector(connector, server, stream)
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))?;

        Ok(TlsStream {
            tls,
            digest: None,
            timing: Default::default(),
        })
    }

    /// Create a new TLS connection from the given `stream`
    ///
    /// Using RustTLS the stream is only returned after the handshake.
    /// The caller does therefor not need to perform [`Self::accept()`].
    pub(crate) async fn from_acceptor(acceptor: &Acceptor, stream: T) -> Result<Self> {
        let tls = InnerStream::from_acceptor(acceptor, stream)
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("tls stream error: {e}"))?;

        Ok(TlsStream {
            tls,
            digest: None,
            timing: Default::default(),
        })
    }
}

impl<S> GetSocketDigest for TlsStream<S>
where
    S: GetSocketDigest,
{
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        self.tls.get_socket_digest()
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.tls.set_socket_digest(socket_digest)
    }
}

impl<S> GetTimingDigest for TlsStream<S>
where
    S: GetTimingDigest,
{
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        let mut ts_vec = self.tls.get_timing_digest();
        ts_vec.push(Some(self.timing.clone()));
        ts_vec
    }
    fn get_read_pending_time(&self) -> Duration {
        self.tls.get_read_pending_time()
    }

    fn get_write_pending_time(&self) -> Duration {
        self.tls.get_write_pending_time()
    }
}

impl<S> GetProxyDigest for TlsStream<S>
where
    S: GetProxyDigest,
{
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        self.tls.get_proxy_digest()
    }
}

impl<T> TlsStream<T> {
    pub fn ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.digest.clone()
    }
}

impl<T> Deref for TlsStream<T> {
    type Target = InnerStream<T>;

    fn deref(&self) -> &Self::Target {
        &self.tls
    }
}

impl<T> DerefMut for TlsStream<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tls
    }
}

impl<T> TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Connect to the remote TLS server as a client
    pub(crate) async fn connect(&mut self) -> Result<()> {
        self.tls.connect().await?;
        self.timing.established_ts = SystemTime::now();
        self.digest = self.tls.digest();
        Ok(())
    }

    /// Finish the TLS handshake from client as a server
    pub(crate) async fn accept(&mut self) -> Result<()> {
        self.tls.accept().await?;
        self.timing.established_ts = SystemTime::now();
        self.digest = self.tls.digest();
        Ok(())
    }
}

impl<T> AsyncRead for TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<IoResult<usize>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<IoResult<()>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<IoResult<()>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<IoResult<usize>> {
        Pin::new(&mut self.tls.stream.as_mut().unwrap()).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl<T> UniqueID for TlsStream<T>
where
    T: UniqueID,
{
    fn id(&self) -> UniqueIDType {
        self.tls.stream.as_ref().unwrap().get_ref().0.id()
    }
}

impl<T> Ssl for TlsStream<T> {
    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.ssl_digest()
    }

    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let st = self.tls.stream.as_ref();
        if let Some(stream) = st {
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

/// Create a new TLS connection from the given `stream`
///
/// The caller needs to perform [`Self::connect()`] or [`Self::accept()`] to perform TLS
/// handshake after.
impl<T: AsyncRead + AsyncWrite + Unpin> InnerStream<T> {
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
        let accept = acceptor.acceptor.accept(stream);

        Ok(InnerStream {
            accept: Some(accept).into(),
            connect: None.into(),
            stream: None,
        })
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> InnerStream<T> {
    /// Connect to the remote TLS server as a client
    pub(crate) async fn connect(&mut self) -> Result<()> {
        let connect = &mut (*self.connect);
        let connect = connect.take().or_err(
            ConnectError,
            "TLS connect not available to perform handshake.",
        )?;

        let stream = connect
            .await
            .or_err(TLSHandshakeFailure, "tls connect error")?;
        self.stream = Some(RusTlsStream::Client(stream));
        Ok(())
    }

    /// Finish the TLS handshake from client as a server
    /// no-op implementation within Rustls, handshake is performed during creation of stream.
    pub(crate) async fn accept(&mut self) -> Result<()> {
        let accept = &mut (*self.accept);
        let accept = accept.take().or_err(
            AcceptError,
            "TLS accept not available to perform handshake.",
        )?;

        let stream = accept
            .await
            .explain_err(TLSHandshakeFailure, |e| format!("tls connect error: {e}"))?;
        self.stream = Some(RusTlsStream::Server(stream));
        Ok(())
    }

    pub(crate) fn digest(&mut self) -> Option<Arc<SslDigest>> {
        Some(Arc::new(SslDigest::from_stream(&self.stream)))
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

impl SslDigest {
    fn from_stream<T>(stream: &Option<RusTlsStream<T>>) -> Self {
        let stream = stream.as_ref().unwrap();
        let (_io, session) = stream.get_ref();
        let protocol = session.protocol_version();
        let cipher_suite = session.negotiated_cipher_suite();
        let peer_certificates = session.peer_certificates();

        let cipher = cipher_suite
            .and_then(|suite| suite.suite().as_str())
            .unwrap_or_default();

        let version = protocol
            .and_then(|proto| proto.as_str())
            .unwrap_or_default();

        let cert_digest = peer_certificates
            .and_then(|certs| certs.first())
            .map(|cert| hash_certificate(cert))
            .unwrap_or_default();

        let (organization, serial_number) = peer_certificates
            .and_then(|certs| certs.first())
            .map(|cert| get_organization_serial_bytes(cert.as_bytes()))
            .transpose()
            .ok()
            .flatten()
            .map(|(organization, serial)| (organization, Some(serial)))
            .unwrap_or_default();

        SslDigest {
            cipher,
            version,
            organization,
            serial_number,
            cert_digest,
        }
    }
}

impl<S> Peek for TlsStream<S> {}
