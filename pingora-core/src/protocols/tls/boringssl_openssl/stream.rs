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

use super::coalesce::WriteCoalescer;
use crate::protocols::digest::TimingDigest;
use crate::protocols::tls::{SslDigest, ALPN};
use crate::protocols::{Peek, Ssl, UniqueID, UniqueIDType};
use crate::tls::{self, ssl, tokio_ssl::SslStream as InnerSsl};
use crate::utils::tls::{get_organization, get_serial};
use log::warn;
use pingora_error::{ErrorType::*, OrErr, Result};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime};
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};

#[cfg(feature = "boringssl")]
use pingora_boringssl as ssl_lib;

#[cfg(feature = "openssl")]
use pingora_openssl as ssl_lib;

use ssl_lib::{hash::MessageDigest, ssl::SslRef};

/// The TLS connection
///
/// As with any OpenSSL or BoringSSL stream, a write that returns `Pending` must be retried
/// with the same bytes, optionally followed by more, because `SSL_write` may already have
/// sealed them into a record. With [write coalescing](Self::enable_write_coalescing), a
/// retry that does not start with the bytes of a coalesced write fails with
/// [`InvalidInput`](io::ErrorKind::InvalidInput) instead of reporting them as written, and
/// the stream writes nothing else until they are retried.
#[derive(Debug)]
pub struct SslStream<T> {
    ssl: InnerSsl<T>,
    digest: Option<Arc<SslDigest>>,
    pub(super) timing: TimingDigest,
    /// `None` unless [`Self::enable_write_coalescing`] was called.
    write_coalescer: Option<WriteCoalescer>,
}

impl<T> SslStream<T>
where
    T: AsyncRead + AsyncWrite + std::marker::Unpin,
{
    /// Create a new TLS connection from the given `stream`
    ///
    /// The caller needs to perform [`Self::connect()`] or [`Self::accept()`] to perform TLS
    /// handshake after.
    pub fn new(ssl: ssl::Ssl, stream: T) -> Result<Self> {
        let ssl = InnerSsl::new(ssl, stream)
            .explain_err(TLSHandshakeFailure, |e| format!("ssl stream error: {e}"))?;

        Ok(SslStream {
            ssl,
            digest: None,
            timing: Default::default(),
            write_coalescer: None,
        })
    }

    /// Connect to the remote TLS server as a client
    pub async fn connect(&mut self) -> Result<(), ssl::Error> {
        Self::clear_error();
        let handshake_start = Instant::now();
        Pin::new(&mut self.ssl).connect().await?;
        self.timing.establishment_duration = Some(handshake_start.elapsed());
        self.timing.established_ts = SystemTime::now();
        self.digest = Some(Arc::new(SslDigest::from_ssl(self.ssl())));
        Ok(())
    }

    /// Finish the TLS handshake from client as a server
    pub async fn accept(&mut self) -> Result<(), ssl::Error> {
        Self::clear_error();
        let handshake_start = Instant::now();
        Pin::new(&mut self.ssl).accept().await?;
        self.timing.establishment_duration = Some(handshake_start.elapsed());
        self.timing.established_ts = SystemTime::now();
        self.digest = Some(Arc::new(SslDigest::from_ssl(self.ssl())));
        Ok(())
    }

    #[inline]
    fn clear_error() {
        let errs = tls::error::ErrorStack::get();
        if !errs.errors().is_empty() {
            warn!("Clearing dirty TLS error stack: {}", errs);
        }
    }
}

impl<T> SslStream<T> {
    pub fn ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.digest.clone()
    }

    /// Attempts to obtain a mutable reference to the SslDigest.
    /// This method returns `None` if the SslDigest is currently held by other references.
    pub(crate) fn ssl_digest_mut(&mut self) -> Option<&mut SslDigest> {
        Arc::get_mut(self.digest.as_mut()?)
    }

    /// Copy the slices of each vectored write into one TLS record of up to 16 KiB when that
    /// takes fewer records than writing them one at a time.
    ///
    /// Without it, a vectored write sends only its first non-empty slice, so h2 puts each
    /// DATA frame of 256 bytes or more into two records and the HTTP/1.1 chunked writer puts
    /// each chunk into three, each costing its own `SSL_write` and usually its own socket
    /// write. Call this before the first write; once enabled, it stays on.
    ///
    /// **Note:** this is _unstable_ and may be renamed or removed in the future.
    pub fn enable_write_coalescing(&mut self) {
        self.write_coalescer
            .get_or_insert_with(WriteCoalescer::default);
    }

    /// Whether [`Self::enable_write_coalescing`] was called.
    ///
    /// **Note:** this is _unstable_ and may be renamed or removed in the future.
    pub fn write_coalescing(&self) -> bool {
        self.write_coalescer.is_some()
    }
}

use std::ops::{Deref, DerefMut};

impl<T> Deref for SslStream<T> {
    type Target = InnerSsl<T>;

    fn deref(&self) -> &Self::Target {
        &self.ssl
    }
}

impl<T> DerefMut for SslStream<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ssl
    }
}

impl<T> AsyncRead for SslStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for SslStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        // A write retried after `Pending` must offer the bytes the coalescer holds.
        self.poll_write_vectored(cx, &[std::io::IoSlice::new(buf)])
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Self::clear_error();
        Pin::new(&mut self.ssl).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Self::clear_error();
        let this = self.get_mut();
        match &mut this.write_coalescer {
            Some(coalescer) => coalescer.poll_write_vectored(Pin::new(&mut this.ssl), cx, bufs),
            None => Pin::new(&mut this.ssl).poll_write_vectored(cx, bufs),
        }
    }

    // TODO: with write coalescing off, the inner stream writes only the first slice of a
    // vectored write, so this should then be `false`. h2 would then copy DATA frames under
    // 1 KiB into its own buffer instead of handing them over as two slices. It stays `true` so
    // that with coalescing off, writes take the same path as before.
    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl<T> UniqueID for SslStream<T>
where
    T: UniqueID,
{
    fn id(&self) -> UniqueIDType {
        self.ssl.get_ref().id()
    }
}

impl<T> Ssl for SslStream<T> {
    fn get_ssl(&self) -> Option<&ssl::SslRef> {
        Some(self.ssl())
    }

    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.ssl_digest()
    }

    /// Return selected ALPN if any
    fn selected_alpn_proto(&self) -> Option<ALPN> {
        let ssl = self.get_ssl()?;
        ALPN::from_wire_selected(ssl.selected_alpn_protocol()?)
    }
}

impl SslDigest {
    pub fn from_ssl(ssl: &SslRef) -> Self {
        let cipher = match ssl.current_cipher() {
            Some(c) => c.name(),
            None => "",
        };

        let (cert_digest, org, sn) = match ssl.peer_certificate() {
            Some(cert) => {
                let cert_digest = match cert.digest(MessageDigest::sha256()) {
                    Ok(c) => c.as_ref().to_vec(),
                    Err(_) => Vec::new(),
                };
                (cert_digest, get_organization(&cert), get_serial(&cert).ok())
            }
            None => (Vec::new(), None, None),
        };

        SslDigest::new(cipher, ssl.version_str(), org, sn, cert_digest)
    }
}

// TODO: implement Peek if needed
impl<T> Peek for SslStream<T> {}

#[cfg(test)]
mod tests {
    use super::SslStream;
    use crate::tls::ssl::{self, SslFiletype};
    use std::io::IoSlice;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::{
        self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf,
    };

    /// TLS `ContentType::application_data`; TLS 1.3 also wraps handshake messages in it.
    const APPLICATION_DATA: u8 = 23;

    /// A transport that keeps a copy of every byte written through it, and makes each write
    /// wait once first, as a full socket does, so that every TLS write is retried.
    struct Tap {
        io: DuplexStream,
        written: Arc<Mutex<Vec<u8>>>,
        stalled: bool,
    }

    impl AsyncRead for Tap {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Tap {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.stalled = !self.stalled;
            if self.stalled {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let result = Pin::new(&mut self.io).poll_write(cx, buf);
            if let Poll::Ready(Ok(n)) = result {
                self.written.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            result
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_shutdown(cx)
        }
    }

    /// Walks the 5-byte TLS record headers (content type, version, big-endian length).
    fn application_data_records(mut wire: &[u8]) -> usize {
        let mut records = 0;
        while let [content_type, _, _, len_hi, len_lo, rest @ ..] = wire {
            if *content_type == APPLICATION_DATA {
                records += 1;
            }
            wire = &rest[usize::from(u16::from_be_bytes([*len_hi, *len_lo]))..];
        }
        records
    }

    /// A context with pingora's partial-write mode, so records are cut as in production, but
    /// without its `ACCEPT_MOVING_WRITE_BUFFER`, so a write retried from a moved buffer fails.
    fn strict_context(server: bool) -> ssl::SslContext {
        let mut ctx = ssl::SslContext::builder(ssl::SslMethod::tls()).unwrap();
        if server {
            let cert = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
            let key = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
            ctx.set_certificate_chain_file(cert).unwrap();
            ctx.set_private_key_file(key, SslFiletype::PEM).unwrap();
        } else {
            ctx.set_verify(ssl::SslVerifyMode::NONE);
        }
        ctx.set_mode(ssl::SslMode::ENABLE_PARTIAL_WRITE);
        ctx.build()
    }

    /// Writes `slices` to a TLS connection with vectored writes, as h2 and the HTTP/1.1
    /// chunked writer do, and returns the application-data records they put on the wire.
    /// Every record goes out through a retry after `Pending`.
    async fn records_for(slices: &[&[u8]], write_coalescing: bool) -> usize {
        let sent = slices.concat();
        let acceptor = {
            let cert = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
            let key = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
            let mut builder =
                ssl::SslAcceptor::mozilla_intermediate_v5(ssl::SslMethod::tls()).unwrap();
            builder.set_certificate_chain_file(cert).unwrap();
            builder.set_private_key_file(key, SslFiletype::PEM).unwrap();
            builder.build()
        };
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let len = sent.len();
        let server = tokio::spawn(async move {
            let mut stream = super::super::server::handshake(&acceptor, server_io)
                .await
                .unwrap();
            let mut received = vec![0; len];
            stream.read_exact(&mut received).await.unwrap();
            received
        });

        let ssl = ssl::Ssl::new(&strict_context(false)).unwrap();
        let written = Arc::new(Mutex::new(Vec::new()));
        let tap = Tap {
            io: client_io,
            written: written.clone(),
            stalled: false,
        };
        let mut client = SslStream::new(ssl, tap).unwrap();
        if write_coalescing {
            client.enable_write_coalescing();
        }
        client.connect().await.unwrap();
        let handshake_len = written.lock().unwrap().len();

        let mut bufs: Vec<IoSlice> = slices.iter().map(|slice| IoSlice::new(slice)).collect();
        let mut bufs = &mut bufs[..];
        while !bufs.is_empty() {
            let n = client.write_vectored(bufs).await.unwrap();
            IoSlice::advance_slices(&mut bufs, n);
        }
        client.flush().await.unwrap();

        assert_eq!(server.await.unwrap(), sent);
        let wire = written.lock().unwrap();
        application_data_records(&wire[handshake_len..])
    }

    /// h2 hands a DATA frame of 256 bytes or more to the transport as two slices, the
    /// buffered frame head and the payload; written separately they cost two records, two
    /// `SSL_write`s and two `SSL_read`s.
    #[tokio::test]
    async fn h2_data_frame_goes_out_in_one_record() {
        assert_eq!(records_for(&[&[1; 256], &[2; 4096]], true).await, 1);
    }

    /// The HTTP/1.1 chunked writer hands each chunk over as size line, data and CRLF.
    #[tokio::test]
    async fn http1_chunk_goes_out_in_one_record() {
        assert_eq!(
            records_for(&[b"1000\r\n", &[3; 4096], b"\r\n"], true).await,
            1
        );
    }

    /// Write coalescing is off unless enabled, and then each slice costs a record, as with
    /// tokio's default `poll_write_vectored`.
    #[tokio::test]
    async fn without_write_coalescing_each_slice_goes_out_in_its_own_record() {
        assert_eq!(records_for(&[&[1; 256], &[2; 4096]], false).await, 2);
        assert_eq!(
            records_for(&[b"1000\r\n", &[3; 4096], b"\r\n"], false).await,
            3
        );
    }

    /// A connected client and server, each writing through a [`Tap`] and with
    /// [`strict_context`], so every write on either side is retried after `Pending` and a retry
    /// from a moved buffer fails.
    async fn retrying_pair(write_coalescing: bool) -> (SslStream<Tap>, SslStream<Tap>) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let stream = |server: bool, io: DuplexStream| {
            let ssl = ssl::Ssl::new(&strict_context(server)).unwrap();
            let tap = Tap {
                io,
                written: Arc::new(Mutex::new(Vec::new())),
                stalled: false,
            };
            let mut stream = SslStream::new(ssl, tap).unwrap();
            if write_coalescing {
                stream.enable_write_coalescing();
            }
            stream
        };
        let mut server = stream(true, server_io);
        let server = tokio::spawn(async move {
            server.accept().await.unwrap();
            server
        });
        let mut client = stream(false, client_io);
        client.connect().await.unwrap();
        (client, server.await.unwrap())
    }

    /// Body piece sizes around the h2 chaining thresholds (256 B, 1 KiB) and one TLS record.
    const PIECES: &[usize] = &[
        1, 255, 256, 300, 1000, 1023, 1024, 4096, 9000, 16375, 16376, 16384, 20000, 65536, 7,
    ];

    /// Sends `body` on an h2 stream, one DATA write per piece of the sizes in [`PIECES`],
    /// starting at the `skip`th.
    fn send_in_pieces(stream: &mut h2::SendStream<bytes::Bytes>, body: &[u8], skip: usize) {
        let mut sent = 0;
        for &len in PIECES.iter().cycle().skip(skip) {
            if sent == body.len() {
                break;
            }
            let end = (sent + len).min(body.len());
            stream
                .send_data(bytes::Bytes::copy_from_slice(&body[sent..end]), false)
                .unwrap();
            sent = end;
        }
        stream.send_data(bytes::Bytes::new(), true).unwrap();
    }

    async fn read_body(mut body: h2::RecvStream) -> Vec<u8> {
        let mut received = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            body.flow_control().release_capacity(chunk.len()).unwrap();
            received.extend_from_slice(&chunk);
        }
        received
    }

    /// Runs `streams` concurrent h2 requests over [`retrying_pair`]; the server echoes each
    /// body back in pieces of other sizes.
    async fn echo_over_h2(write_coalescing: bool, streams: usize) {
        let (client, server) = retrying_pair(write_coalescing).await;
        let server = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server).await.unwrap();
            let mut echoes = Vec::new();
            // Once every request is done, the client goes away without a TLS close_notify,
            // which the server may report as an error here.
            while let Some(Ok((request, mut respond))) = connection.accept().await {
                echoes.push(tokio::spawn(async move {
                    let body = read_body(request.into_body()).await;
                    let mut stream = respond
                        .send_response(http::Response::new(()), false)
                        .unwrap();
                    send_in_pieces(&mut stream, &body, 3);
                }));
            }
            for echo in echoes {
                echo.await.unwrap();
            }
        });
        let (send_request, connection) = h2::client::handshake(client).await.unwrap();
        let connection = tokio::spawn(connection);
        let mut requests = Vec::new();
        for i in 0..streams {
            let mut send_request = send_request.clone().ready().await.unwrap();
            requests.push(tokio::spawn(async move {
                let body: Vec<u8> = (0..300_000).map(|j| (j ^ i) as u8).collect();
                let request = http::Request::post("https://pingora.org/")
                    .body(())
                    .unwrap();
                let (response, mut stream) = send_request.send_request(request, false).unwrap();
                send_in_pieces(&mut stream, &body, i);
                let echoed = read_body(response.await.unwrap().into_body()).await;
                assert!(echoed == body, "stream {i} came back changed");
            }));
        }
        for request in requests {
            request.await.unwrap();
        }
        drop(send_request);
        connection.await.unwrap().unwrap();
        server.await.unwrap();
    }

    /// h2 retries a `Pending` write with the same bytes, so a coalesced retry never fails with
    /// `InvalidInput`. While a passed-through write is `Pending`, h2 buffers more frames, so
    /// its retry can arrive with more slices than the first attempt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn h2_bodies_survive_retried_writes() {
        for write_coalescing in [false, true] {
            for _ in 0..5 {
                // A retry from a moved buffer wedges the connection rather than failing it.
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    echo_over_h2(write_coalescing, 16),
                )
                .await
                .expect("h2 exchange over retried TLS writes timed out");
            }
        }
    }
}
