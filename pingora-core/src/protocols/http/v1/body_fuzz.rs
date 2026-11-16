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

//! Randomized tests for [`BodyReader`].
//!
//! Each case builds a content-length, chunked or close-delimited body from a seed and feeds it to
//! a [`BodyReader`] through a stream that returns random read sizes and random
//! [`Poll::Pending`]s. Half the cases read through an L4 [`Stream`] with a random read-ahead
//! capacity instead of reading the stream directly. The driver sometimes drops the read future
//! while it is pending and starts a new one, as a timeout or `select!` would. Every case is
//! checked against the same properties:
//!
//! - The reader returns exactly the encoded payload.
//! - Every byte past the end of the message is either in the overread buffer or can still be read
//!   from the stream, in order.
//! - A read error inside the message, or content-length or chunked input that ends early, is never
//!   reported as a complete body. A read error after the end of such a message is never surfaced.
//! - Corrupted chunked framing never panics or loops.
//! - Bytes sharing an allocation with an owned preread are never written.
//!
//! `cargo test` runs a fixed set of seeds. For a longer run with fresh seeds:
//!
//! ```text
//! PINGORA_BODY_FUZZ_CASES=200000 cargo test -p pingora-core --lib body_fuzz -- --ignored --nocapture
//! ```
//!
//! A failure prints its case seed, and `PINGORA_BODY_FUZZ_CASE=<seed>` with the same command
//! replays that one case. Seeds only replay with the same version of `rand`.

use super::body::BodyReader;
use crate::protocols::l4::stream::{L4BufferSettings, Stream};
use crate::protocols::l4::virt::{VirtualSockOpt, VirtualSocket, VirtualSocketStream};
use bytes::BytesMut;
use futures::task::noop_waker;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const KIB: usize = 1024;
/// The reader rejects a partial chunk head longer than 8 KiB, and needs room to read the rest of
/// one, so a smaller body buffer cannot parse every valid body.
const MIN_BODY_BUF_SIZE: usize = 8 * KIB + 1;
/// Longest chunk head the generator writes, counting the size, extensions and CRLF.
const MAX_CHUNK_HEAD: usize = 8 * KIB;
/// Trailer bytes the generator aims for at most. The reader rejects more than 64 KiB, and the
/// last trailer line can overshoot this by up to 8 KiB.
const MAX_TRAILERS: usize = 48 * KIB;
/// Longest preread, roughly the most a header read can leave behind.
const MAX_PREREAD: usize = 70 * KIB;
/// Streams at most this long may use 1 to 7 byte reads.
const MAX_TINY_READ_STREAM: usize = 24 * KIB;
/// The stream returns a ready result after at most this many [`Poll::Pending`]s in a row, so every
/// case makes progress.
const MAX_PENDINGS_IN_A_ROW: u32 = 3;
/// Stands in for the header bytes that share an allocation with an owned preread.
const HEADER: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

const CI_SEED: u64 = 0x5eed_b0d1_f022_0001;
const CI_CASES: u64 = 1500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    ContentLength,
    Chunked,
    UntilClose,
}

#[derive(Clone, Copy, Debug)]
enum Input {
    /// The whole message and its excess bytes, then EOF.
    Valid,
    /// EOF at this wire offset, which is inside the message.
    Truncated(usize),
    /// Reads fail from this wire offset on, or from the first read if the preread covers it.
    ReadError(usize),
    /// The wire byte at this offset, inside the chunked framing or payload, is replaced.
    Corrupted(usize),
}

#[derive(Clone, Copy, Debug)]
enum ReadSizes {
    /// Fill the destination, like a socket with plenty of data waiting.
    Unlimited,
    /// Mostly 16 KiB reads, like TLS returning one record per read.
    TlsRecords,
    /// Random sizes up to 16 bytes, 1 KiB or 64 KiB.
    Mixed,
    /// 1 to 7 bytes.
    Tiny,
}

impl ReadSizes {
    fn next(self, rng: &mut StdRng) -> usize {
        match self {
            ReadSizes::Unlimited => usize::MAX,
            ReadSizes::TlsRecords if rng.gen_bool(0.8) => 16 * KIB,
            ReadSizes::TlsRecords => rng.gen_range(1..=16 * KIB),
            ReadSizes::Mixed => {
                let max = *[16, KIB, 64 * KIB].choose(rng).unwrap();
                rng.gen_range(1..=max)
            }
            ReadSizes::Tiny => rng.gen_range(1..=7),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Transport {
    /// The body reader reads the stream directly.
    Direct,
    /// The body reader reads an L4 [`Stream`] with this read-ahead capacity.
    L4(usize),
}

/// What a [`FuzzStream`] returned, shared with the driver, which cannot reach the stream once it is
/// inside a [`Stream`].
#[derive(Default)]
struct StreamEvents {
    pendings: AtomicU64,
    errors: AtomicU64,
}

/// A stream over `data` that returns random read sizes, random [`Poll::Pending`]s, and
/// optionally a read error from a given offset on.
struct FuzzStream {
    data: Vec<u8>,
    pos: usize,
    rng: StdRng,
    sizes: ReadSizes,
    pending_per_mille: u32,
    pendings_in_a_row: u32,
    error_at: Option<usize>,
    events: Arc<StreamEvents>,
}

impl std::fmt::Debug for FuzzStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuzzStream")
            .field("len", &self.data.len())
            .field("pos", &self.pos)
            .finish()
    }
}

impl VirtualSocket for FuzzStream {
    fn set_socket_option(&self, _opt: VirtualSockOpt) -> io::Result<()> {
        Ok(())
    }
}

impl AsyncWrite for FuzzStream {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for FuzzStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pendings_in_a_row < MAX_PENDINGS_IN_A_ROW
            && this.rng.gen_ratio(this.pending_per_mille, 1000)
        {
            this.pendings_in_a_row += 1;
            this.events.pendings.fetch_add(1, Ordering::Relaxed);
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        this.pendings_in_a_row = 0;
        if this.error_at.is_some_and(|at| this.pos >= at) {
            this.events.errors.fetch_add(1, Ordering::Relaxed);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected read error",
            )));
        }
        // Stop short of the error offset, so the error arrives exactly there.
        let end = this
            .error_at
            .map_or(this.data.len(), |at| at.min(this.data.len()));
        let n = this
            .sizes
            .next(&mut this.rng)
            .min(buf.remaining())
            .min(end - this.pos);
        buf.put_slice(&this.data[this.pos..this.pos + n]);
        this.pos += n;
        Poll::Ready(Ok(()))
    }
}

struct Case {
    framing: Framing,
    upstream: bool,
    body_buf_size: Option<usize>,
    payload: Vec<u8>,
    /// `wire[..message_len]` is the encoded message and the rest is excess, such as the start of
    /// a pipelined request.
    wire: Vec<u8>,
    message_len: usize,
    chunking: String,
    input: Input,
    preread: usize,
    owned_preread: bool,
    /// Give the owned preread at least 32 KiB of capacity, the default body read size, so body
    /// reads can reuse the allocation it shares with the header bytes.
    owned_preread_32k: bool,
    sizes: ReadSizes,
    transport: Transport,
    pending_per_mille: u32,
    cancel_per_mille: u32,
}

#[derive(Debug, Default)]
struct Stats {
    cases: u64,
    content_length: u64,
    chunked: u64,
    until_close: u64,
    l4: u64,
    truncated: u64,
    read_errors: u64,
    /// Read error cases where the error reached the body reader, rather than only a read after
    /// the body ended.
    read_errors_in_body: u64,
    corrupted: u64,
    pendings: u64,
    cancellations: u64,
    /// Valid upstream content-length cases with excess bytes on the wire, and how many of them
    /// the reader noticed. Noticing is best effort: it depends on the excess arriving in the
    /// same read as the end of the body.
    upstream_excess: u64,
    upstream_excess_noticed: u64,
}

impl Stats {
    fn add(&mut self, other: &Stats) {
        self.cases += other.cases;
        self.content_length += other.content_length;
        self.chunked += other.chunked;
        self.until_close += other.until_close;
        self.l4 += other.l4;
        self.truncated += other.truncated;
        self.read_errors += other.read_errors;
        self.read_errors_in_body += other.read_errors_in_body;
        self.corrupted += other.corrupted;
        self.pendings += other.pendings;
        self.cancellations += other.cancellations;
        self.upstream_excess += other.upstream_excess;
        self.upstream_excess_noticed += other.upstream_excess_noticed;
    }
}

fn pick_len(rng: &mut StdRng, body_buf_size: usize) -> usize {
    match rng.gen_range(0..20) {
        0..=2 => 0,
        3..=8 => rng.gen_range(1..2 * KIB),
        // Lengths around multiples of the read sizes the reader and the stream use, where off-by-one
        // errors in read limits show up.
        9..=15 => {
            let unit = *[16 * KIB, 32 * KIB, 64 * KIB, body_buf_size]
                .choose(rng)
                .unwrap();
            let delta = *[-1025, -1024, -1023, -2, -1, 0, 0, 0, 1, 2, 1023, 1024, 1025]
                .choose(rng)
                .unwrap();
            (unit * rng.gen_range(1..=4)).saturating_add_signed(delta)
        }
        _ => rng.gen_range(0..200 * KIB),
    }
}

fn push_letters(rng: &mut StdRng, out: &mut Vec<u8>, len: usize) {
    out.extend((0..len).map(|_| rng.gen_range(b'a'..=b'z')));
}

/// Append one chunk-size line, recording its offsets as framing.
fn push_chunk_head(rng: &mut StdRng, wire: &mut Vec<u8>, framing: &mut Vec<usize>, size: usize) {
    let start = wire.len();
    for _ in 0..rng.gen_range(0..=2) {
        wire.push(b'0');
    }
    let digits = if rng.gen_bool(0.5) {
        format!("{size:x}")
    } else {
        format!("{size:X}")
    };
    wire.extend_from_slice(digits.as_bytes());
    match rng.gen_range(0..20) {
        0..=12 => {}
        // Whitespace is allowed between the size and the end of the line or an extension.
        13 => wire.extend_from_slice(b" \t"),
        14..=18 => {
            if rng.gen_bool(0.3) {
                wire.push(b' ');
            }
            wire.extend_from_slice(if rng.gen_bool(0.5) {
                b";name=value"
            } else {
                b";a;b=\"quoted ; value\""
            });
        }
        _ => {
            wire.extend_from_slice(b";x=");
            let room = MAX_CHUNK_HEAD - (wire.len() - start) - 2;
            let len = rng.gen_range(0..=room);
            push_letters(rng, wire, len);
        }
    }
    wire.extend_from_slice(b"\r\n");
    framing.extend(start..wire.len());
}

/// Encode `payload` as a chunked body, returning the chunking description.
fn push_chunked(
    rng: &mut StdRng,
    payload: &[u8],
    body_buf_size: usize,
    wire: &mut Vec<u8>,
    framing: &mut Vec<usize>,
) -> String {
    // Chunks up to the body buffer size line chunk boundaries up with read boundaries.
    let max_chunk = *[16, KIB, 16 * KIB, body_buf_size, 70 * KIB]
        .choose(rng)
        .unwrap();
    let mut rest = payload;
    let mut chunks = 0;
    while !rest.is_empty() {
        let n = rng.gen_range(1..=max_chunk.min(rest.len()));
        push_chunk_head(rng, wire, framing, n);
        wire.extend_from_slice(&rest[..n]);
        framing.extend(wire.len()..wire.len() + 2);
        wire.extend_from_slice(b"\r\n");
        rest = &rest[n..];
        chunks += 1;
    }
    push_chunk_head(rng, wire, framing, 0);
    let trailers_start = wire.len();
    if rng.gen_bool(0.2) {
        let budget = if rng.gen_bool(0.2) {
            rng.gen_range(0..MAX_TRAILERS)
        } else {
            rng.gen_range(0..200)
        };
        let mut i = 0;
        while wire.len() - trailers_start < budget {
            let start = wire.len();
            write!(DisplayVec(wire), "x-trailer-{i}: ").unwrap();
            let room = budget.saturating_sub(wire.len() - trailers_start);
            let len = rng.gen_range(0..=room.min(8 * KIB));
            push_letters(rng, wire, len);
            wire.extend_from_slice(b"\r\n");
            framing.extend(start..wire.len());
            i += 1;
        }
    }
    framing.extend(wire.len()..wire.len() + 2);
    wire.extend_from_slice(b"\r\n");
    format!(
        "{chunks} chunks of at most {max_chunk} bytes, {} trailer bytes",
        wire.len() - trailers_start - 2
    )
}

/// Lets `write!` append to a `Vec<u8>`.
struct DisplayVec<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for DisplayVec<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

impl Case {
    fn generate(rng: &mut StdRng) -> Case {
        let framing = match rng.gen_range(0..10) {
            0..=3 => Framing::ContentLength,
            4..=8 => Framing::Chunked,
            _ => Framing::UntilClose,
        };
        let upstream = rng.gen_bool(0.5);
        let body_buf_size = rng
            .gen_bool(0.25)
            .then(|| rng.gen_range(MIN_BODY_BUF_SIZE..=70 * KIB));
        let effective_buf_size =
            body_buf_size.unwrap_or_else(|| BodyReader::new(upstream).body_buf_size);

        let mut payload = vec![0; pick_len(rng, effective_buf_size)];
        rng.fill(&mut payload[..]);

        let mut wire = Vec::with_capacity(payload.len() + 4 * KIB);
        let mut framing_offsets = Vec::new();
        let chunking = match framing {
            Framing::Chunked => push_chunked(
                rng,
                &payload,
                effective_buf_size,
                &mut wire,
                &mut framing_offsets,
            ),
            _ => {
                wire.extend_from_slice(&payload);
                String::new()
            }
        };
        let message_len = wire.len();
        if framing != Framing::UntilClose {
            let excess = match rng.gen_range(0..10) {
                0..=4 => 0,
                5..=7 => rng.gen_range(1..64),
                _ => rng.gen_range(64..3000),
            };
            let start = wire.len();
            wire.resize(start + excess, 0);
            rng.fill(&mut wire[start..]);
        }

        let input = match rng.gen_range(0..100) {
            0..=69 => Input::Valid,
            70..=81 if message_len > 0 => Input::Truncated(rng.gen_range(0..message_len)),
            82..=89 => Input::ReadError(rng.gen_range(0..=wire.len())),
            90..=99 if framing == Framing::Chunked => {
                let offset = if rng.gen_bool(0.7) {
                    *framing_offsets.choose(rng).unwrap()
                } else {
                    rng.gen_range(0..message_len)
                };
                Input::Corrupted(offset)
            }
            _ => Input::Valid,
        };
        if let Input::Corrupted(offset) = input {
            wire[offset] = rng.gen();
        }
        let available = match input {
            Input::Truncated(cut) => cut,
            _ => wire.len(),
        };
        let preread = if rng.gen_bool(0.3) {
            0
        } else {
            rng.gen_range(0..=available.min(MAX_PREREAD))
        };
        let stream_len = available - preread;

        let sizes = match rng.gen_range(0..4) {
            0 => ReadSizes::Unlimited,
            1 => ReadSizes::TlsRecords,
            3 if stream_len <= MAX_TINY_READ_STREAM => ReadSizes::Tiny,
            _ => ReadSizes::Mixed,
        };
        let transport = if rng.gen_bool(0.5) {
            Transport::Direct
        } else {
            // Body reads at least as large as the read-ahead capacity bypass it.
            Transport::L4(match rng.gen_range(0..6) {
                0 => L4BufferSettings::default().read_capacity(),
                1 => 0,
                2 => rng.gen_range(1..=64),
                3 => effective_buf_size,
                4 => effective_buf_size + 1,
                _ => rng.gen_range(1..=70 * KIB),
            })
        };

        Case {
            framing,
            upstream,
            body_buf_size,
            payload,
            wire,
            message_len,
            chunking,
            input,
            preread,
            owned_preread: rng.gen_bool(0.5),
            owned_preread_32k: rng.gen_bool(0.5),
            sizes,
            transport,
            pending_per_mille: *[0, 100, 500].choose(rng).unwrap(),
            cancel_per_mille: *[0, 300, 1000].choose(rng).unwrap(),
        }
    }

    fn describe(&self) -> String {
        format!(
            "{:?} {} body_buf_size={:?} payload={} message={} wire={} {} input={:?} \
             preread={} owned_preread={} owned_preread_32k={} reads={:?} transport={:?} \
             pending_per_mille={} cancel_per_mille={}",
            self.framing,
            if self.upstream {
                "upstream"
            } else {
                "downstream"
            },
            self.body_buf_size,
            self.payload.len(),
            self.message_len,
            self.wire.len(),
            self.chunking,
            self.input,
            self.preread,
            self.owned_preread,
            self.owned_preread_32k,
            self.sizes,
            self.transport,
            self.pending_per_mille,
            self.cancel_per_mille,
        )
    }

    fn run(&self, rng: &mut StdRng, stats: &mut Stats) -> Result<(), String> {
        let mut reader = BodyReader::new(self.upstream);
        if let Some(size) = self.body_buf_size {
            reader.body_buf_size = size;
        }
        let preread = &self.wire[..self.preread];

        // An owned preread is split off a buffer that starts with header bytes, as the header
        // parser hands it over. The header half must come through untouched.
        let mut header = BytesMut::new();
        let owned = self.owned_preread.then(|| {
            let spare = if self.owned_preread_32k {
                (32 * KIB).max(preread.len())
            } else {
                preread.len()
            };
            header = BytesMut::with_capacity(HEADER.len() + spare);
            header.extend_from_slice(HEADER);
            header.extend_from_slice(preread);
            header.split_off(HEADER.len())
        });
        let len = self.payload.len();
        match (self.framing, owned) {
            (Framing::ContentLength, None) => reader.init_content_length(len, preread),
            (Framing::ContentLength, Some(b)) => reader.init_content_length_owned(len, b),
            (Framing::Chunked, None) => reader.init_chunked(preread),
            (Framing::Chunked, Some(b)) => reader.init_chunked_owned(b),
            (Framing::UntilClose, None) => reader.init_close_delimited(preread),
            (Framing::UntilClose, Some(b)) => reader.init_close_delimited_owned(b),
        }

        let end = match self.input {
            Input::Truncated(cut) => cut,
            _ => self.wire.len(),
        };
        let events = Arc::new(StreamEvents::default());
        let mut fuzz = FuzzStream {
            data: self.wire[self.preread..end].to_vec(),
            pos: 0,
            rng: StdRng::from_rng(&mut *rng).unwrap(),
            sizes: self.sizes,
            pending_per_mille: self.pending_per_mille,
            pendings_in_a_row: 0,
            error_at: match self.input {
                Input::ReadError(at) => Some(at.saturating_sub(self.preread)),
                _ => None,
            },
            events: events.clone(),
        };
        let result = match self.transport {
            Transport::Direct => self.drive(&mut reader, &mut fuzz, &events, rng, stats),
            Transport::L4(read_capacity) => {
                let mut stream = Stream::from(VirtualSocketStream::new(Box::new(fuzz)));
                stream.set_buffer(L4BufferSettings::new(read_capacity, 0));
                self.drive(&mut reader, &mut stream, &events, rng, stats)
            }
        };
        stats.pendings += events.pendings.load(Ordering::Relaxed);
        result?;
        if self.owned_preread {
            check_bytes("header sharing the owned preread", &header, HEADER)?;
        }
        Ok(())
    }

    /// Read the body from `stream`, dropping pending reads at random, and check the outcome.
    fn drive<S>(
        &self,
        reader: &mut BodyReader,
        stream: &mut S,
        events: &StreamEvents,
        rng: &mut StdRng,
        stats: &mut Stats,
    ) -> Result<(), String>
    where
        S: AsyncRead + Unpin + Send,
    {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let max_polls = 8 * (self.wire.len() + 1000);
        let mut polls = 0;
        let mut body = Vec::new();
        let outcome = loop {
            let polled = {
                let mut read = pin!(reader.read_body(stream));
                loop {
                    polls += 1;
                    if polls > max_polls {
                        return Err(format!("no result after {max_polls} polls"));
                    }
                    match read.as_mut().poll(&mut cx) {
                        Poll::Ready(result) => break Some(result),
                        Poll::Pending if rng.gen_ratio(self.cancel_per_mille, 1000) => {
                            stats.cancellations += 1;
                            break None;
                        }
                        Poll::Pending => {}
                    }
                }
            };
            match polled {
                None => {}
                Some(Ok(Some(buf_ref))) => {
                    body.extend_from_slice(reader.get_body(&buf_ref));
                    // Decoding never produces more bytes than the wire carries, so this catches a
                    // stream that replays data before it exhausts memory.
                    if body.len() > self.wire.len() {
                        return Err(format!(
                            "{} body bytes from a {} byte wire",
                            body.len(),
                            self.wire.len()
                        ));
                    }
                }
                Some(Ok(None)) => break Ok(()),
                Some(Err(e)) => break Err(e),
            }
        };

        let body_read_failed = events.errors.load(Ordering::Relaxed) > 0;
        stats.read_errors_in_body += u64::from(body_read_failed);
        if let Err(e) = &outcome {
            if reader.body_complete() {
                return Err(format!("failed with a complete body: {e}"));
            }
        }
        let len = self.payload.len();
        match (self.input, &outcome) {
            (_, Ok(())) if body_read_failed => {
                Err("body completed after the stream returned an error".into())
            }
            (Input::Corrupted(_), _) => Ok(()),
            (Input::Valid | Input::ReadError(_), Ok(())) => {
                match pin!(reader.read_body(stream)).poll(&mut cx) {
                    Poll::Ready(Ok(None)) => {}
                    other => return Err(format!("read after the end returned {other:?}")),
                }
                let (rest, read_failed) =
                    read_rest(stream, &mut cx, rng, self.wire.len(), max_polls)?;
                self.check_complete(reader, &body, &rest, read_failed, stats)
            }
            (Input::Valid, Err(e)) => Err(format!("valid input failed: {e}")),
            (Input::Truncated(cut), Ok(())) if self.framing == Framing::UntilClose => {
                check_bytes("body", &body, &self.payload[..cut])
            }
            (Input::Truncated(_), Ok(())) => {
                Err("input that ends early was reported as a complete body".into())
            }
            (Input::ReadError(at), Err(e))
                if self.framing != Framing::UntilClose && at >= self.message_len =>
            {
                Err(format!(
                    "failed although the whole message arrived before the read error: {e}"
                ))
            }
            (Input::Truncated(_) | Input::ReadError(_), Err(_)) => {
                if body.len() > len {
                    return Err(format!(
                        "{} body bytes from a {len} byte payload",
                        body.len()
                    ));
                }
                check_bytes("body prefix", &body, &self.payload[..body.len()])
            }
        }
    }

    /// Check a body that was reported complete. `rest` is what could still be read from the
    /// stream afterwards, cut short by an injected error if `read_failed`.
    fn check_complete(
        &self,
        reader: &BodyReader,
        body: &[u8],
        rest: &[u8],
        read_failed: bool,
        stats: &mut Stats,
    ) -> Result<(), String> {
        check_bytes("body", body, &self.payload)?;
        if !reader.body_complete() {
            return Err(format!("finished in state {:?}", reader.body_state));
        }
        let overread = reader.get_body_overread().unwrap_or_default();
        let mut after = overread.to_vec();
        after.extend_from_slice(rest);
        let excess = &self.wire[self.message_len..];
        let expected = if read_failed {
            &excess[..after.len().min(excess.len())]
        } else {
            excess
        };
        check_bytes("bytes after the body", &after, expected)?;
        if self.upstream && self.framing == Framing::ContentLength && !excess.is_empty() {
            stats.upstream_excess += 1;
            stats.upstream_excess_noticed += u64::from(reader.has_bytes_overread());
        }
        Ok(())
    }
}

/// Read `stream` to EOF in random sizes, failing past `limit` bytes. Returns the bytes and whether
/// a read failed.
fn read_rest<S: AsyncRead + Unpin>(
    stream: &mut S,
    cx: &mut Context<'_>,
    rng: &mut StdRng,
    limit: usize,
    max_polls: usize,
) -> Result<(Vec<u8>, bool), String> {
    let mut rest = Vec::new();
    let mut buf = vec![0; 64 * KIB];
    for _ in 0..max_polls {
        let len = rng.gen_range(1..=buf.len());
        let mut read_buf = ReadBuf::new(&mut buf[..len]);
        match Pin::new(&mut *stream).poll_read(cx, &mut read_buf) {
            Poll::Pending => {}
            Poll::Ready(Ok(())) if read_buf.filled().is_empty() => return Ok((rest, false)),
            Poll::Ready(Ok(())) => rest.extend_from_slice(read_buf.filled()),
            Poll::Ready(Err(_)) => return Ok((rest, true)),
        }
        if rest.len() > limit {
            return Err(format!(
                "{} bytes after the body, more than the whole wire",
                rest.len()
            ));
        }
    }
    Err(format!("no EOF after {max_polls} polls"))
}

fn check_bytes(what: &str, actual: &[u8], expected: &[u8]) -> Result<(), String> {
    if actual == expected {
        return Ok(());
    }
    let first_diff = actual
        .iter()
        .zip(expected)
        .position(|(a, e)| a != e)
        .unwrap_or(actual.len().min(expected.len()));
    Err(format!(
        "{what}: got {} bytes, expected {}, first difference at {first_diff}",
        actual.len(),
        expected.len()
    ))
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic")
}

fn run_case(seed: u64, stats: &mut Stats) {
    let mut rng = StdRng::seed_from_u64(seed);
    let case = Case::generate(&mut rng);
    match catch_unwind(AssertUnwindSafe(|| case.run(&mut rng, stats))) {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => panic!("body fuzz case {seed:#x}: {failure}\n{}", case.describe()),
        Err(panic) => panic!(
            "body fuzz case {seed:#x} panicked: {}\n{}",
            panic_message(&*panic),
            case.describe()
        ),
    }
    stats.cases += 1;
    match case.framing {
        Framing::ContentLength => stats.content_length += 1,
        Framing::Chunked => stats.chunked += 1,
        Framing::UntilClose => stats.until_close += 1,
    }
    if let Transport::L4(_) = case.transport {
        stats.l4 += 1;
    }
    match case.input {
        Input::Valid => {}
        Input::Truncated(_) => stats.truncated += 1,
        Input::ReadError(_) => stats.read_errors += 1,
        Input::Corrupted(_) => stats.corrupted += 1,
    }
}

/// Run `cases` cases spread over all cores. Their seeds count up from a starting point drawn from
/// `run_seed`, so runs with nearby seeds, such as consecutive CI pipeline IDs, share no cases.
fn run_cases(run_seed: u64, cases: u64) -> Stats {
    let first_case = StdRng::seed_from_u64(run_seed).gen::<u64>();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let mut stats = Stats::default();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || {
                    let mut stats = Stats::default();
                    for i in (t..cases).step_by(threads as usize) {
                        run_case(first_case.wrapping_add(i), &mut stats);
                    }
                    stats
                })
            })
            .collect();
        for worker in workers {
            match worker.join() {
                Ok(worker_stats) => stats.add(&worker_stats),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    stats
}

fn env_u64(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    let parsed = match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    };
    Some(parsed.unwrap_or_else(|_| panic!("{name}={value} is not a number")))
}

#[test]
fn body_reader_fuzz_fixed_seeds() {
    let stats = run_cases(CI_SEED, CI_CASES);
    assert_eq!(stats.cases, CI_CASES);
    // Make sure a generator change has not quietly stopped producing some kinds of case.
    for (kind, count) in [
        ("content-length", stats.content_length),
        ("chunked", stats.chunked),
        ("close-delimited", stats.until_close),
        ("L4 transport", stats.l4),
        ("truncated", stats.truncated),
        ("read error", stats.read_errors),
        ("read error during the body", stats.read_errors_in_body),
        ("corrupted", stats.corrupted),
        ("cancellation", stats.cancellations),
        ("upstream excess", stats.upstream_excess),
    ] {
        assert!(count > 0, "no {kind} cases: {stats:?}");
    }
}

#[test]
#[ignore]
fn body_reader_fuzz_soak() {
    if let Some(seed) = env_u64("PINGORA_BODY_FUZZ_CASE") {
        let mut stats = Stats::default();
        run_case(seed, &mut stats);
        eprintln!("{stats:?}");
        return;
    }
    let cases = env_u64("PINGORA_BODY_FUZZ_CASES").unwrap_or(100_000);
    let seed = env_u64("PINGORA_BODY_FUZZ_SEED").unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    });
    eprintln!("body fuzz: {cases} cases from run seed {seed:#x}");
    let stats = run_cases(seed, cases);
    eprintln!("{stats:?}");
}
