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

//! Coalesces the slices of a vectored write to a TLS stream into fewer records.
//!
//! Neither pingora-boringssl's `boring_tokio` nor `tokio-openssl` implements vectored writes,
//! so tokio's default [`AsyncWrite::poll_write_vectored`] hands only the first non-empty
//! slice to `SSL_write`. Each slice then costs its own TLS record, `SSL_write` and usually
//! socket write on this side, and its own `SSL_read` on the peer. h2 writes every DATA frame
//! of 256 bytes or more as two slices: its buffered frames, ending with the frame head topped
//! up to 256 bytes from the payload, and the rest of the payload. The HTTP/1.1 chunked writer
//! writes each chunk as three: size line, data and CRLF.
//!
//! Coalescing only pays when the slices fit in fewer records together than apart. A DATA
//! frame with a payload of up to 16,375 bytes goes out in one record instead of two, but a
//! full 16 KiB frame, 16,393 bytes with its head, takes two either way and is not copied.

// TODO: upstream a vectored write to tokio-openssl and to boring's tokio-boring (which
// `boring_tokio` reimplements), then remove this module.
// `without_write_coalescing_each_slice_goes_out_in_its_own_record` starts failing once the
// inner stream writes a vectored write's slices into fewer records.

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;

/// The most plaintext one TLS record carries, and the most BoringSSL's `SSL_write` takes in
/// one call in partial-write mode; OpenSSL may take more. A session that negotiated smaller
/// records can take less; see [`WriteCoalescer::record_len`].
const MAX_RECORD_PLAINTEXT: usize = 16 * 1024;

/// Copies the slices of a vectored write into one buffer, up to one TLS record, when that
/// saves a record, and writes the buffer with a single `SSL_write`. Otherwise it passes the
/// first non-empty slice through uncopied.
///
/// A TLS stream that returns `Pending` may already have sealed the bytes it was given into
/// a record, so the retry must offer the same bytes and, unless the context sets
/// `SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER`, from the same address. The coalescer keeps a
/// coalesced buffer until the TLS stream accepts it and offers that same buffer on the
/// retry, and passes a slice it passed through before through again. A retry whose slices
/// no longer start with the bytes of a coalesced buffer fails rather than reporting stale
/// bytes as written.
pub(super) struct WriteCoalescer {
    /// The coalesced bytes of a write the TLS stream returned `Pending` for, and empty
    /// otherwise, so an idle connection holds no buffer.
    pending: Vec<u8>,
    /// Whether the TLS stream returned `Pending` for a slice passed through uncopied. Its
    /// retry is passed through too, from the caller's buffer, rather than copied to a new one.
    passthrough_pending: bool,
    /// The most plaintext the TLS stream takes in one write: [`MAX_RECORD_PLAINTEXT`], or
    /// less once a coalesced write shows that the session uses smaller records. Coalescing
    /// more than this only copies bytes that later writes copy again. It never grows back, so
    /// one short write caps copies for the rest of the stream.
    record_len: usize,
}

impl Default for WriteCoalescer {
    fn default() -> Self {
        WriteCoalescer {
            pending: Vec::new(),
            passthrough_pending: false,
            record_len: MAX_RECORD_PLAINTEXT,
        }
    }
}

impl WriteCoalescer {
    /// Writes a prefix of `bufs` to `tls` and returns how many bytes of `bufs` it wrote.
    pub(super) fn poll_write_vectored<W: AsyncWrite + ?Sized>(
        &mut self,
        mut tls: Pin<&mut W>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.pending.is_empty() {
            let total: usize = bufs.iter().map(|buf| buf.len()).sum();
            let first = bufs
                .iter()
                .find(|buf| !buf.is_empty())
                .map_or(&[][..], |buf| &**buf);
            if self.passthrough_pending || !saves_a_record(first.len(), total, self.record_len) {
                let result = tls.as_mut().poll_write(cx, first);
                self.passthrough_pending = result.is_pending();
                return result;
            }
            let len = total.min(self.record_len);
            self.pending.reserve_exact(len);
            for buf in bufs {
                let take = buf.len().min(len - self.pending.len());
                self.pending.extend_from_slice(&buf[..take]);
            }
        } else if !starts_with(bufs, &self.pending) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TLS write retried without the bytes of the pending write",
            )));
        }
        let result = tls.poll_write(cx, &self.pending);
        if let Poll::Ready(written) = &result {
            // The buffer holds at most one full-size record, so taking only part of it shows
            // the most the stream takes per write, as when the session uses smaller records.
            if let Ok(n) = written {
                if (1..self.pending.len()).contains(n) {
                    self.record_len = *n;
                }
            }
            self.pending = Vec::new();
        }
        result
    }
}

impl std::fmt::Debug for WriteCoalescer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteCoalescer")
            .field("pending", &self.pending.len())
            .field("passthrough_pending", &self.passthrough_pending)
            .field("record_len", &self.record_len)
            .finish()
    }
}

/// Whether writing `total` bytes at once takes fewer records of `record_len` than writing
/// the `first` slice alone and the rest after it. When it does not, coalescing only costs a
/// copy.
fn saves_a_record(first: usize, total: usize, record_len: usize) -> bool {
    let records = |len: usize| len.div_ceil(record_len);
    first < total && records(total) < 1 + records(total - first)
}

/// Whether the concatenation of `bufs` starts with `prefix`.
fn starts_with(bufs: &[IoSlice<'_>], mut prefix: &[u8]) -> bool {
    for buf in bufs {
        if prefix.is_empty() {
            break;
        }
        let n = buf.len().min(prefix.len());
        if buf[..n] != prefix[..n] {
            return false;
        }
        prefix = &prefix[n..];
    }
    prefix.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each write it is given and accepts it whole, or returns `Pending` for the
    /// next `pending` writes, as a TLS stream does when its socket is full. Like BoringSSL's
    /// `SSL_write` in partial-write mode, it accepts at most one record, of `record_len` if set.
    #[derive(Default)]
    struct Recorder {
        writes: Vec<(*const u8, Vec<u8>)>,
        pending: usize,
        record_len: Option<usize>,
    }

    impl Recorder {
        fn record_len(&self) -> usize {
            self.record_len.unwrap_or(MAX_RECORD_PLAINTEXT)
        }
    }

    impl AsyncWrite for Recorder {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.writes.push((buf.as_ptr(), buf.to_vec()));
            if this.pending > 0 {
                this.pending -= 1;
                return Poll::Pending;
            }
            Poll::Ready(Ok(buf.len().min(this.record_len())))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_once(
        coalescer: &mut WriteCoalescer,
        tls: &mut Recorder,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        coalescer.poll_write_vectored(Pin::new(tls), &mut cx, bufs)
    }

    /// A frame head topped up to 256 bytes, as h2 buffers it, and the payload after it.
    fn frame(payload: usize) -> (Vec<u8>, Vec<u8>) {
        let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(256 + payload).collect();
        let (head, rest) = bytes.split_at(256);
        (head.to_vec(), rest.to_vec())
    }

    #[test]
    fn frame_head_and_payload_go_out_in_one_write() {
        let (head, payload) = frame(4096);
        let (mut coalescer, mut tls) = (WriteCoalescer::default(), Recorder::default());

        let written = poll_once(
            &mut coalescer,
            &mut tls,
            &[IoSlice::new(&head), IoSlice::new(&payload)],
        );

        assert!(matches!(written, Poll::Ready(Ok(4352))));
        assert_eq!(tls.writes.len(), 1);
        assert_eq!(tls.writes[0].1, [head, payload].concat());
        assert_eq!(
            coalescer.pending.capacity(),
            0,
            "no buffer outlives the write"
        );
    }

    #[test]
    fn coalesced_write_stops_at_one_record() {
        let (head, payload) = frame(20_000);
        let (mut coalescer, mut tls) = (WriteCoalescer::default(), Recorder::default());

        let written = poll_once(
            &mut coalescer,
            &mut tls,
            &[IoSlice::new(&head), IoSlice::new(&payload)],
        );

        assert!(matches!(written, Poll::Ready(Ok(MAX_RECORD_PLAINTEXT))));
        let all = [head, payload].concat();
        assert_eq!(tls.writes[0].1, all[..MAX_RECORD_PLAINTEXT]);
    }

    /// A 64 KiB DATA frame is 65,545 bytes with its head, five records whether or not the
    /// head is coalesced, so the head goes out uncopied.
    #[test]
    fn head_goes_alone_when_coalescing_saves_no_record() {
        let (head, payload) = frame(65_289);
        let (mut coalescer, mut tls) = (WriteCoalescer::default(), Recorder::default());

        let written = poll_once(
            &mut coalescer,
            &mut tls,
            &[IoSlice::new(&head), IoSlice::new(&payload)],
        );

        assert!(matches!(written, Poll::Ready(Ok(256))));
        assert_eq!(tls.writes[0].0, head.as_ptr());
    }

    #[test]
    fn single_slice_passes_through_uncopied() {
        let buf = vec![7u8; 300];
        let (mut coalescer, mut tls) = (WriteCoalescer::default(), Recorder::default());

        let written = poll_once(
            &mut coalescer,
            &mut tls,
            &[IoSlice::new(&[]), IoSlice::new(&buf)],
        );

        assert!(matches!(written, Poll::Ready(Ok(300))));
        assert_eq!(tls.writes[0].0, buf.as_ptr());
    }

    #[test]
    fn retry_after_pending_offers_the_same_bytes_from_the_same_address() {
        let (head, payload) = frame(4096);
        let bufs = [IoSlice::new(&head), IoSlice::new(&payload)];
        let mut coalescer = WriteCoalescer::default();
        let mut tls = Recorder {
            pending: 1,
            ..Recorder::default()
        };

        assert!(poll_once(&mut coalescer, &mut tls, &bufs).is_pending());
        assert!(matches!(
            poll_once(&mut coalescer, &mut tls, &bufs),
            Poll::Ready(Ok(4352))
        ));

        let [(first_ptr, first), (retry_ptr, retry)] = &tls.writes[..] else {
            panic!("expected a write and its retry: {:?}", tls.writes.len());
        };
        assert_eq!(retry_ptr, first_ptr);
        assert_eq!(retry, first);
        assert_eq!(
            coalescer.pending.capacity(),
            0,
            "no buffer outlives the write"
        );
    }

    /// h2 keeps buffering frames while a write of its buffer is `Pending`, so the retry can
    /// offer a longer buffer at the same address with a payload slice after it.
    #[test]
    fn retry_of_a_slice_passed_through_is_passed_through_from_the_same_address() {
        let (frames, payload) = frame(4096);
        let mut coalescer = WriteCoalescer::default();
        let mut tls = Recorder {
            pending: 1,
            ..Recorder::default()
        };
        assert!(poll_once(&mut coalescer, &mut tls, &[IoSlice::new(&frames[..100])]).is_pending());

        let retry = poll_once(
            &mut coalescer,
            &mut tls,
            &[IoSlice::new(&frames), IoSlice::new(&payload)],
        );

        assert!(matches!(retry, Poll::Ready(Ok(256))));
        assert_eq!(tls.writes[1].0, frames.as_ptr());
    }

    #[test]
    fn retry_with_other_bytes_fails_instead_of_writing_stale_ones() {
        let (head, payload) = frame(4096);
        let mut coalescer = WriteCoalescer::default();
        let mut tls = Recorder {
            pending: 1,
            ..Recorder::default()
        };
        assert!(poll_once(
            &mut coalescer,
            &mut tls,
            &[IoSlice::new(&head), IoSlice::new(&payload)]
        )
        .is_pending());

        let changed = vec![0u8; payload.len()];
        let retry = poll_once(
            &mut coalescer,
            &mut tls,
            &[IoSlice::new(&head), IoSlice::new(&changed)],
        );

        assert!(matches!(retry, Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::InvalidInput));
        assert_eq!(tls.writes.len(), 1, "the stale bytes are not offered again");
    }

    /// After `set_max_send_fragment`, or on an OpenSSL server that honours a client's
    /// `max_fragment_length`, a session's records can be as small as 512 bytes, and
    /// BoringSSL's `SSL_write` then takes one of them per call. An HTTP/1.1 chunk keeps two
    /// slices, its data and CRLF, until the last write, so the coalescer must not copy the
    /// rest of the chunk on every one of those writes.
    #[test]
    fn copies_at_most_one_record_after_the_stream_takes_smaller_records() {
        let (size_line, data, crlf) = (b"4000\r\n".to_vec(), vec![3; 16 * 1024], b"\r\n".to_vec());
        let mut coalescer = WriteCoalescer::default();
        let mut tls = Recorder {
            record_len: Some(512),
            ..Recorder::default()
        };

        let mut bufs = [
            IoSlice::new(&size_line),
            IoSlice::new(&data),
            IoSlice::new(&crlf),
        ];
        let mut bufs = &mut bufs[..];
        while !bufs.is_empty() {
            let Poll::Ready(Ok(n)) = poll_once(&mut coalescer, &mut tls, bufs) else {
                panic!("the recorder accepts every write");
            };
            IoSlice::advance_slices(&mut bufs, n);
        }

        let accepted: Vec<u8> = tls
            .writes
            .iter()
            .flat_map(|(_, bytes)| &bytes[..bytes.len().min(512)])
            .copied()
            .collect();
        assert_eq!(accepted, [&size_line[..], &data, &crlf].concat());
        let caller = [&size_line[..], &data, &crlf];
        let copied: usize = tls
            .writes
            .iter()
            .filter(|(ptr, _)| !caller.iter().any(|buf| buf.as_ptr_range().contains(ptr)))
            .map(|(_, bytes)| bytes.len())
            .sum();
        assert!(
            copied <= MAX_RECORD_PLAINTEXT + 512,
            "copied {copied} bytes"
        );
    }

    #[test]
    fn saves_a_record_only_when_the_tail_spills_past_the_head() {
        const R: usize = MAX_RECORD_PLAINTEXT;
        assert!(saves_a_record(256, 256 + 4096, R));
        assert!(saves_a_record(256, 2 * R, R));
        assert!(!saves_a_record(256, 256 + 65_289, R));
        assert!(!saves_a_record(300, 300, R));
        assert!(!saves_a_record(R, R + 10, R));
        // 512-byte records: a 256-byte head plus a 4 KiB payload takes 9 records either way.
        assert!(!saves_a_record(256, 256 + 4096, 512));
        assert!(saves_a_record(6, 8, 512));
    }
}
