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

use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::{debug, trace, warn};
use pingora_error::{
    Error,
    ErrorType::{self, *},
    OrErr, Result,
};
use std::fmt::Debug;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocols::l4::stream::{
    async_write_vec::{poll_write_all_buf, poll_write_vec_all_buf},
    AsyncWriteVec,
};
use crate::utils::BufRef;

// TODO: make this dynamically adjusted
const BODY_BUFFER_SIZE: usize = 1024 * 64;
// limit how much incomplete chunk-size and chunk-ext to buffer
const PARTIAL_CHUNK_HEAD_LIMIT: usize = 1024 * 8;
// Trailers: https://datatracker.ietf.org/doc/html/rfc9112#section-7.1.2
// TODO: proper trailer handling and parsing
// generally trailers are an uncommonly used HTTP/1.1 feature, this is a somewhat
// arbitrary cap on trailer size after the 0 chunk size (like header buf)
const TRAILER_SIZE_LIMIT: usize = 1024 * 64;

const LAST_CHUNK: &[u8; 5] = &[b'0', CR, LF, CR, LF];
const CR: u8 = b'\r';
const LF: u8 = b'\n';
const CRLF: &[u8; 2] = &[CR, LF];
// This is really the CRLF end of the last trailer (or 0 chunk), + the last CRLF.
const TRAILERS_END: &[u8; 4] = &[CR, LF, CR, LF];

pub const INVALID_CHUNK: ErrorType = ErrorType::new("InvalidChunk");
pub const INVALID_TRAILER_END: ErrorType = ErrorType::new("InvalidTrailerEnd");
pub const PREMATURE_BODY_END: ErrorType = ErrorType::new("PrematureBodyEnd");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseState {
    ToStart,
    // Complete: total size (contetn-length)
    Complete(usize),
    // Partial: size read, remaining size (content-length)
    Partial(usize, usize),
    // Chunked: Chunked encoding, prior to the final 0\r\n chunk.
    // size read, next to read in current buf start, read in current buf start, remaining chunked size to read from IO
    Chunked(usize, usize, usize, usize),
    // ChunkedFinal: Final section once the 0\r\n chunk is read.
    // size read, trailer sizes parsed so far, use existing buf end, trailers end read
    ChunkedFinal(usize, usize, usize, u8),
    // Done: done but there is error, size read
    Done(usize),
    // UntilClose: read until connection closed, size read
    UntilClose(usize),
}

type PS = ParseState;

impl ParseState {
    pub fn finish(&self, additional_bytes: usize) -> Self {
        match self {
            PS::Partial(read, to_read) => PS::Complete(read + to_read),
            PS::Chunked(read, _, _, _) => PS::Complete(read + additional_bytes),
            PS::ChunkedFinal(read, _, _, _) => PS::Complete(read + additional_bytes),
            PS::UntilClose(read) => PS::Complete(read + additional_bytes),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn done(&self, additional_bytes: usize) -> Self {
        match self {
            PS::Partial(read, _) => PS::Done(read + additional_bytes),
            PS::Chunked(read, _, _, _) => PS::Done(read + additional_bytes),
            PS::ChunkedFinal(read, _, _, _) => PS::Done(read + additional_bytes),
            PS::UntilClose(read) => PS::Done(read + additional_bytes),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn read_final_chunk(&self, remaining_buf_size: usize) -> Self {
        match self {
            PS::Chunked(read, _, _, _) => {
                // The BodyReader is currently expected to copy the remaining buf
                // into self.body_buf.
                //
                // the 2 == the CRLF from the last chunk-size, 0 + CRLF
                // because ChunkedFinal is looking for CRLF + CRLF to end
                // the whole message.
                // This extra 2 bytes technically ends up cutting into the max trailers size,
                // which we consider fine for now until full trailers support.
                PS::ChunkedFinal(*read, 0, remaining_buf_size, 2)
            }
            PS::ChunkedFinal(..) => panic!("already read final chunk"),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn partial_chunk(&self, bytes_read: usize, bytes_to_read: usize) -> Self {
        match self {
            PS::Chunked(read, _, _, _) => PS::Chunked(read + bytes_read, 0, 0, bytes_to_read),
            PS::ChunkedFinal(..) => panic!("chunked transactions not applicable after final chunk"),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn multi_chunk(&self, bytes_read: usize, buf_start_index: usize) -> Self {
        match self {
            PS::Chunked(read, _, buf_end, _) => {
                PS::Chunked(read + bytes_read, buf_start_index, *buf_end, 0)
            }
            PS::ChunkedFinal(..) => panic!("chunked transactions not applicable after final chunk"),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn partial_chunk_head(&self, head_end: usize, head_size: usize) -> Self {
        match self {
            /* inform reader to read more to form a legal chunk */
            PS::Chunked(read, _, _, _) => PS::Chunked(*read, 0, head_end, head_size),
            PS::ChunkedFinal(..) => panic!("chunked transactions not applicable after final chunk"),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn new_buf(&self, buf_end: usize) -> Self {
        match self {
            PS::Chunked(read, _, _, _) => PS::Chunked(*read, 0, buf_end, 0),
            PS::ChunkedFinal(..) => panic!("chunked transactions not applicable after final chunk"),
            _ => self.clone(), /* invalid transaction */
        }
    }
}

pub struct BodyReader {
    pub body_state: ParseState,
    pub body_buf: Option<BytesMut>,
    pub body_buf_size: usize,
    rewind_buf_len: usize,
    upstream: bool,
    body_buf_overread: Option<BytesMut>,
}

impl BodyReader {
    pub fn new(upstream: bool) -> Self {
        BodyReader {
            body_state: PS::ToStart,
            body_buf: None,
            body_buf_size: BODY_BUFFER_SIZE,
            rewind_buf_len: 0,
            upstream,
            body_buf_overread: None,
        }
    }

    pub fn need_init(&self) -> bool {
        matches!(self.body_state, PS::ToStart)
    }

    pub fn reinit(&mut self) {
        self.body_state = PS::ToStart;
    }

    fn prepare_buf(&mut self, buf_to_rewind: &[u8]) {
        let mut body_buf = BytesMut::with_capacity(self.body_buf_size);
        if !buf_to_rewind.is_empty() {
            self.rewind_buf_len = buf_to_rewind.len();
            // TODO: this is still 1 copy. Make it zero
            body_buf.put_slice(buf_to_rewind);
        }
        if self.body_buf_size > buf_to_rewind.len() {
            //body_buf.resize(self.body_buf_size, 0);
            unsafe {
                body_buf.set_len(self.body_buf_size);
            }
        }
        self.body_buf = Some(body_buf);
    }

    pub fn init_chunked(&mut self, buf_to_rewind: &[u8]) {
        self.body_state = PS::Chunked(0, 0, 0, 0);
        self.prepare_buf(buf_to_rewind);
    }

    pub fn init_content_length(&mut self, cl: usize, buf_to_rewind: &[u8]) {
        match cl {
            0 => {
                self.body_state = PS::Complete(0);
                // Store any extra bytes that were read as overread
                if !buf_to_rewind.is_empty() {
                    let mut overread = BytesMut::with_capacity(buf_to_rewind.len());
                    overread.put_slice(buf_to_rewind);
                    self.body_buf_overread = Some(overread);
                }
            }
            _ => {
                self.prepare_buf(buf_to_rewind);
                self.body_state = PS::Partial(0, cl);
            }
        }
    }

    pub fn init_close_delimited(&mut self, buf_to_rewind: &[u8]) {
        self.prepare_buf(buf_to_rewind);
        self.body_state = PS::UntilClose(0);
    }

    /// Convert how we interpret the remainder of the body to read until close.
    /// This is used for responses without explicit framing (e.g., HTTP/1.0 responses).
    ///
    /// Does nothing if already in close-delimited mode.
    pub fn convert_to_close_delimited(&mut self) {
        if matches!(self.body_state, PS::UntilClose(_)) {
            // nothing to do, already in close-delimited mode
            return;
        }

        if self.rewind_buf_len == 0 {
            // take any extra bytes and send them as-is,
            // reset body counter
            let extra = self.body_buf_overread.take();
            let buf = extra.as_deref().unwrap_or_default();
            self.prepare_buf(buf);
        } // if rewind_buf_len is not 0, body read has not yet been polled
        self.body_state = PS::UntilClose(0);
    }

    pub fn get_body(&self, buf_ref: &BufRef) -> &[u8] {
        // TODO: these get_*() could panic. handle them better
        buf_ref.get(self.body_buf.as_ref().unwrap())
    }

    #[allow(dead_code)]
    pub fn get_body_overread(&self) -> Option<&[u8]> {
        self.body_buf_overread.as_deref()
    }

    pub fn has_bytes_overread(&self) -> bool {
        self.get_body_overread().is_some_and(|b| !b.is_empty())
    }

    pub fn body_done(&self) -> bool {
        matches!(self.body_state, PS::Complete(_) | PS::Done(_))
    }

    pub fn body_empty(&self) -> bool {
        self.body_state == PS::Complete(0)
    }

    fn finish_body_buf(&mut self, end_of_body: usize, total_read: usize) {
        let body_buf_mut = self.body_buf.as_mut().expect("must have read body buf");
        // remove unused buffer
        body_buf_mut.truncate(total_read);
        let overread_bytes = body_buf_mut.split_off(end_of_body);
        self.body_buf_overread = (!overread_bytes.is_empty()).then_some(overread_bytes);
    }

    pub async fn read_body<S>(&mut self, stream: &mut S) -> Result<Option<BufRef>>
    where
        S: AsyncRead + Unpin + Send,
    {
        match self.body_state {
            PS::Complete(_) => Ok(None),
            PS::Done(_) => Ok(None),
            PS::Partial(_, _) => self.do_read_body(stream).await,
            PS::Chunked(..) => self.do_read_chunked_body(stream).await,
            PS::ChunkedFinal(..) => self.do_read_chunked_body_final(stream).await,
            PS::UntilClose(_) => self.do_read_body_until_closed(stream).await,
            PS::ToStart => panic!("need to init BodyReader first"),
        }
    }

    pub async fn do_read_body<S>(&mut self, stream: &mut S) -> Result<Option<BufRef>>
    where
        S: AsyncRead + Unpin + Send,
    {
        let mut body_buf = self.body_buf.as_deref_mut().unwrap();
        let mut n = self.rewind_buf_len;
        self.rewind_buf_len = 0; // we only need to read rewind data once
        if n == 0 {
            // downstream should not discard remaining data if peer sent more.
            if !self.upstream {
                if let PS::Partial(_, to_read) = self.body_state {
                    if to_read < body_buf.len() {
                        body_buf = &mut body_buf[..to_read];
                    }
                }
            }
            /* Need to actually read */
            n = stream
                .read(body_buf)
                .await
                .or_err(ReadError, "when reading body")?;
        }
        match self.body_state {
            PS::Partial(read, to_read) => {
                debug!(
                    "BodyReader body_state: {:?}, read data from IO: {n}",
                    self.body_state
                );
                if n == 0 {
                    self.body_state = PS::Done(read);
                    Error::e_explain(ConnectionClosed, format!(
                        "Peer prematurely closed connection with {} bytes of body remaining to read",
                        to_read
                    ))
                } else if n >= to_read {
                    if n > to_read {
                        warn!(
                            "Peer sent more data then expected: extra {}\
                               bytes, discarding them",
                            n - to_read
                        )
                    }
                    self.body_state = PS::Complete(read + to_read);
                    self.finish_body_buf(to_read, n);
                    Ok(Some(BufRef::new(0, to_read)))
                } else {
                    self.body_state = PS::Partial(read + n, to_read - n);
                    Ok(Some(BufRef::new(0, n)))
                }
            }
            _ => panic!("wrong body state: {:?}", self.body_state),
        }
    }

    pub async fn do_read_body_until_closed<S>(&mut self, stream: &mut S) -> Result<Option<BufRef>>
    where
        S: AsyncRead + Unpin + Send,
    {
        let body_buf = self.body_buf.as_deref_mut().unwrap();
        let mut n = self.rewind_buf_len;
        self.rewind_buf_len = 0; // we only need to read rewind data once
        if n == 0 {
            /* Need to actually read */
            n = stream
                .read(body_buf)
                .await
                .or_err(ReadError, "when reading body")?;
        }
        match self.body_state {
            PS::UntilClose(read) => {
                if n == 0 {
                    self.body_state = PS::Complete(read);
                    Ok(None)
                } else {
                    self.body_state = PS::UntilClose(read + n);
                    Ok(Some(BufRef::new(0, n)))
                }
            }
            _ => panic!("wrong body state: {:?}", self.body_state),
        }
    }

    pub async fn do_read_chunked_body<S>(&mut self, stream: &mut S) -> Result<Option<BufRef>>
    where
        S: AsyncRead + Unpin + Send,
    {
        match self.body_state {
            PS::Chunked(
                total_read,
                existing_buf_start,
                mut existing_buf_end,
                mut expecting_from_io,
            ) => {
                if existing_buf_start == 0 {
                    // read a new buf from IO
                    let body_buf = self.body_buf.as_deref_mut().unwrap();
                    if existing_buf_end == 0 {
                        existing_buf_end = self.rewind_buf_len;
                        self.rewind_buf_len = 0; // we only need to read rewind data once
                        if existing_buf_end == 0 {
                            existing_buf_end = stream
                                .read(body_buf)
                                .await
                                .or_err(ReadError, "when reading body")?;
                        }
                    } else {
                        /* existing_buf_end != 0 this is partial chunk head */
                        /* copy the #expecting_from_io bytes until index existing_buf_end
                         * to the front and read more to form a valid chunk head.
                         * existing_buf_end is the end of the partial head and
                         * expecting_from_io is the len of it */
                        body_buf
                            .copy_within(existing_buf_end - expecting_from_io..existing_buf_end, 0);
                        let new_bytes = stream
                            .read(&mut body_buf[expecting_from_io..])
                            .await
                            .or_err(ReadError, "when reading body")?;
                        if new_bytes == 0 {
                            self.body_state = self.body_state.done(0);
                            return Error::e_explain(
                                ConnectionClosed,
                                format!(
                                    "Connection prematurely closed without the termination chunk \
                                    (partial chunk head), read {total_read} bytes"
                                ),
                            );
                        }

                        /* more data is read, extend the buffer */
                        existing_buf_end = expecting_from_io + new_bytes;
                        expecting_from_io = 0;
                    }
                    self.body_state = self.body_state.new_buf(existing_buf_end);
                }
                if existing_buf_end == 0 {
                    self.body_state = self.body_state.done(0);
                    Error::e_explain(
                        ConnectionClosed,
                        format!(
                            "Connection prematurely closed without the termination chunk, \
                            read {total_read} bytes"
                        ),
                    )
                } else {
                    if expecting_from_io > 0 {
                        let body_buf = self.body_buf.as_ref().unwrap();
                        trace!(
                            "partial chunk payload, expecting_from_io: {}, \
                                existing_buf_end {}, buf: {:?}",
                            expecting_from_io,
                            existing_buf_end,
                            self.body_buf.as_ref().unwrap()[..existing_buf_end].escape_ascii()
                        );

                        // partial chunk payload, will read more
                        if expecting_from_io >= existing_buf_end + 2 {
                            // not enough (doesn't contain CRLF end)
                            self.body_state = self.body_state.partial_chunk(
                                existing_buf_end,
                                expecting_from_io - existing_buf_end,
                            );
                            return Ok(Some(BufRef::new(0, existing_buf_end)));
                        }
                        /* could be expecting DATA + CRLF or just CRLF */
                        let payload_size = expecting_from_io.saturating_sub(2);
                        /* expecting_from_io < existing_buf_end + 2 */
                        let need_lf_only = expecting_from_io == 1; // otherwise we need the whole CRLF
                        if expecting_from_io > existing_buf_end {
                            // potentially:
                            // | CR | LF |
                            //      |    |
                            // (existing_buf_end)
                            //           |
                            //           (expecting_from_io)
                            if payload_size < existing_buf_end {
                                Self::validate_crlf(
                                    &mut self.body_state,
                                    &body_buf[payload_size..existing_buf_end],
                                    need_lf_only,
                                    false,
                                )?;
                            }
                        } else {
                            // expecting_from_io <= existing_buf_end
                            // chunk CRLF end should end here
                            assert!(Self::validate_crlf(
                                &mut self.body_state,
                                &body_buf[payload_size..expecting_from_io],
                                need_lf_only,
                                false,
                            )?);
                        }
                        if expecting_from_io >= existing_buf_end {
                            self.body_state = self
                                .body_state
                                .partial_chunk(payload_size, expecting_from_io - existing_buf_end);

                            return Ok(Some(BufRef::new(0, payload_size)));
                        }

                        /* expecting_from_io < existing_buf_end */
                        self.body_state =
                            self.body_state.multi_chunk(payload_size, expecting_from_io);

                        return Ok(Some(BufRef::new(0, payload_size)));
                    }
                    let (buf_res, last_chunk_size_end) =
                        self.parse_chunked_buf(existing_buf_start, existing_buf_end)?;
                    if buf_res.is_some() {
                        if let Some(idx) = last_chunk_size_end {
                            // just read the last 0 + CRLF, but not final end CRLF
                            // copy the rest of the buffer to the start of the body_buf
                            // so we can parse the remaining bytes as trailers / end
                            let body_buf = self.body_buf.as_deref_mut().unwrap();
                            trace!(
                                "last chunk size end buf {:?}",
                                &body_buf[..existing_buf_end].escape_ascii(),
                            );
                            body_buf.copy_within(idx..existing_buf_end, 0);
                        }
                    }
                    Ok(buf_res)
                }
            }
            _ => panic!("wrong body state: {:?}", self.body_state),
        }
    }

    // Returns: BufRef of next body chunk,
    // terminating chunk-size index end if read completely (0 + CRLF).
    // Note input indices are absolute (to body_buf).
    fn parse_chunked_buf(
        &mut self,
        buf_index_start: usize,
        buf_index_end: usize,
    ) -> Result<(Option<BufRef>, Option<usize>)> {
        let buf = &self.body_buf.as_ref().unwrap()[buf_index_start..buf_index_end];
        let chunk_status = httparse::parse_chunk_size(buf);
        match chunk_status {
            Ok(status) => {
                match status {
                    httparse::Status::Complete((payload_index, chunk_size)) => {
                        // TODO: Check chunk_size overflow
                        trace!(
                            "Got size {chunk_size}, payload_index: {payload_index}, chunk: {:?}",
                            String::from_utf8_lossy(buf).escape_default(),
                        );
                        let chunk_size = chunk_size as usize;
                        // https://github.com/seanmonstar/httparse/issues/149
                        // httparse does not treat zero-size chunk differently, it does not check
                        // that terminating chunk is 0 + double CRLF
                        if chunk_size == 0 {
                            /* terminating chunk, also need to handle trailer. */
                            let chunk_end_index = payload_index + 2;
                            return if chunk_end_index <= buf.len()
                                && buf[payload_index..chunk_end_index] == CRLF[..]
                            {
                                // full terminating CRLF MAY exist in current buf
                                // Skip ChunkedFinal state and go directly to Complete
                                // as optimization.
                                self.body_state = self.body_state.finish(0);
                                self.finish_body_buf(
                                    buf_index_start + chunk_end_index,
                                    buf_index_end,
                                );
                                Ok((None, Some(buf_index_start + payload_index)))
                            } else {
                                // Indicate start of parsing final chunked trailers,
                                // with remaining buf to read
                                self.body_state = self.body_state.read_final_chunk(
                                    buf_index_end - (buf_index_start + payload_index),
                                );

                                Ok((
                                    Some(BufRef::new(0, 0)),
                                    Some(buf_index_start + payload_index),
                                ))
                            };
                        }
                        // chunk-size CRLF [payload_index] byte*[chunk_size] CRLF
                        let data_end_index = payload_index + chunk_size;
                        let chunk_end_index = data_end_index + 2;
                        if chunk_end_index >= buf.len() {
                            // no multi chunk in this buf
                            let actual_size = if data_end_index > buf.len() {
                                buf.len() - payload_index
                            } else {
                                chunk_size
                            };

                            let crlf_start = chunk_end_index.saturating_sub(2);
                            if crlf_start < buf.len() {
                                Self::validate_crlf(
                                    &mut self.body_state,
                                    &buf[crlf_start..],
                                    false,
                                    false,
                                )?;
                            }
                            // else need to read more to get to CRLF

                            self.body_state = self
                                .body_state
                                .partial_chunk(actual_size, chunk_end_index - buf.len());
                            return Ok((
                                Some(BufRef::new(buf_index_start + payload_index, actual_size)),
                                None,
                            ));
                        }
                        /* got multiple chunks, return the first */
                        assert!(Self::validate_crlf(
                            &mut self.body_state,
                            &buf[data_end_index..chunk_end_index],
                            false,
                            false,
                        )?);
                        self.body_state = self
                            .body_state
                            .multi_chunk(chunk_size, buf_index_start + chunk_end_index);
                        Ok((
                            Some(BufRef::new(buf_index_start + payload_index, chunk_size)),
                            None,
                        ))
                    }
                    httparse::Status::Partial => {
                        if buf.len() > PARTIAL_CHUNK_HEAD_LIMIT {
                            // https://datatracker.ietf.org/doc/html/rfc9112#name-chunk-extensions
                            // "A server ought to limit the total length of chunk extensions received"
                            // The buf.len() here is the total length of chunk-size + chunk-ext seen
                            // so far. This check applies to both server and client
                            self.body_state = self.body_state.done(0);
                            Error::e_explain(INVALID_CHUNK, "Chunk ext over limit")
                        } else {
                            self.body_state =
                                self.body_state.partial_chunk_head(buf_index_end, buf.len());
                            Ok((Some(BufRef::new(0, 0)), None))
                        }
                    }
                }
            }
            Err(e) => {
                let context = format!("Invalid chunked encoding: {e:?}");
                debug!(
                    "{context}, {:?}",
                    String::from_utf8_lossy(buf).escape_default()
                );
                self.body_state = self.body_state.done(0);
                Error::e_explain(INVALID_CHUNK, context)
            }
        }
    }

    pub async fn do_read_chunked_body_final<S>(&mut self, stream: &mut S) -> Result<Option<BufRef>>
    where
        S: AsyncRead + Unpin + Send,
    {
        // parse section after last-chunk: https://datatracker.ietf.org/doc/html/rfc9112#section-7.1
        // This is the section after the final chunk we're trying to read, which can include
        // HTTP1 trailers (currently we just discard them).
        // Really we are just waiting for a consecutive CRLF + CRLF to end the body.
        match self.body_state {
            PS::ChunkedFinal(read, trailers_read, existing_buf_end, end_read) => {
                let body_buf = self.body_buf.as_deref_mut().unwrap();
                let (buf, n) = if existing_buf_end != 0 {
                    // finish rest of buf that was read with Chunked state
                    // existing_buf_end is non-zero only once
                    self.body_state = PS::ChunkedFinal(read, trailers_read, 0, end_read);
                    (&body_buf[..existing_buf_end], existing_buf_end)
                } else {
                    let n = stream
                        .read(body_buf)
                        .await
                        .or_err(ReadError, "when reading trailers end")?;

                    (&body_buf[..n], n)
                };

                if n == 0 {
                    self.body_state = PS::Done(read);
                    return Error::e_explain(
                        ConnectionClosed,
                        format!(
                            "Connection prematurely closed without the termination chunk, \
                            read {read} bytes, {trailers_read} trailer bytes"
                        ),
                    );
                }

                let mut start = 0;
                // try to find end within the current IO buffer
                while start < n {
                    // Adjusts body state through each iteration to add trailers read
                    // Each iteration finds the next CR or LF to advance the buf
                    let (trailers_read, end_read) = match self.body_state {
                        PS::ChunkedFinal(_, new_trailers_read, _, new_end_read) => {
                            (new_trailers_read, new_end_read)
                        }
                        _ => unreachable!(),
                    };

                    let mut buf = &buf[start..n];
                    trace!(
                        "Parsing chunk end for buf {:?}",
                        String::from_utf8_lossy(buf).escape_default(),
                    );

                    if end_read == 0 {
                        // find the next CRLF sequence / potential end
                        let (trailers_read, no_crlf) =
                            if let Some(p) = buf.iter().position(|b| *b == CR || *b == LF) {
                                buf = &buf[p..];
                                start += p;
                                (trailers_read + p, false)
                            } else {
                                // consider this all trailer bytes
                                (trailers_read + (n - start), true)
                            };

                        if trailers_read > TRAILER_SIZE_LIMIT {
                            self.body_state = self.body_state.done(0);
                            return Error::e_explain(
                                INVALID_TRAILER_END,
                                "Trailer size over limit",
                            );
                        }

                        self.body_state = PS::ChunkedFinal(read, trailers_read, 0, 0);

                        if no_crlf {
                            // break and allow polling read body again
                            break;
                        }
                    }
                    match Self::parse_trailers_end(&mut self.body_state, buf)? {
                        TrailersEndParseState::NotEnd(next_parse_index) => {
                            trace!(
                                "Parsing chunk end for buf {:?}, resume at {next_parse_index}",
                                String::from_utf8_lossy(buf).escape_default(),
                            );

                            start += next_parse_index;
                        }
                        TrailersEndParseState::Complete(end_idx) => {
                            trace!(
                                "Parsing chunk end for buf {:?}, finished at {end_idx}",
                                String::from_utf8_lossy(buf).escape_default(),
                            );

                            self.finish_body_buf(start + end_idx, n);
                            return Ok(None);
                        }
                    }
                }
            }
            _ => panic!("wrong body state: {:?}", self.body_state),
        }
        // indicate final section is not done
        Ok(Some(BufRef(0, 0)))
    }

    // Parses up to one CRLF at a time to determine if, given the body state,
    // we've parsed a full trailer end.
    // Panics if empty buffer is given.
    fn parse_trailers_end(
        body_state: &mut ParseState,
        buf: &[u8],
    ) -> Result<TrailersEndParseState> {
        assert!(!buf.is_empty(), "parse_trailers_end given empty buffer");

        match body_state.clone() {
            PS::ChunkedFinal(read, trailers_read, _, end_read) => {
                // Look at the body buf we just read and see if it matches
                // the ending CRLF + CRLF sequence.
                let end_read = end_read as usize;
                assert!(end_read < TRAILERS_END.len());
                let to_read = std::cmp::min(buf.len(), TRAILERS_END.len() - end_read);
                let buf = &buf[..to_read];

                // If the start of the buf is not CRLF and we are not in the middle of reading a
                // valid CRLF sequence, return to let caller seek for next CRLF
                if end_read % 2 == 0 && buf[0] != CR && buf[0] != LF {
                    trace!(
                        "parse trailers end {:?}, not CRLF sequence",
                        String::from_utf8_lossy(buf).escape_default(),
                    );
                    *body_state = PS::ChunkedFinal(read, trailers_read + end_read, 0, 0);
                    return Ok(TrailersEndParseState::NotEnd(0));
                }
                // Check for malformed CRLF in trailers (or final end of trailers section)
                let next_parse_index = match end_read {
                    0 | 2 => {
                        // expect start with CR
                        if Self::validate_crlf(body_state, buf, false, true)? {
                            // found CR + LF
                            2
                        } else {
                            // read CR at least
                            1
                        }
                    }
                    1 | 3 => {
                        // assert: only way this can return false is with an empty buffer
                        assert!(Self::validate_crlf(body_state, buf, true, true)?);
                        1
                    }
                    _ => unreachable!(),
                };
                let next_end_read = end_read + next_parse_index;
                let finished = next_end_read == TRAILERS_END.len();
                if finished {
                    trace!(
                        "parse trailers end {:?}, complete {next_end_read}",
                        String::from_utf8_lossy(buf).escape_default(),
                    );
                    *body_state = PS::Complete(read);
                    Ok(TrailersEndParseState::Complete(next_parse_index))
                } else {
                    // either we read the end of one trailer and another one follows,
                    // or trailer end CRLF sequence so far is valid but we need more bytes
                    // to determine if more CRLF actually follows
                    trace!(
                        "parse trailers end {:?}, resume at {next_parse_index}",
                        String::from_utf8_lossy(buf).escape_default(),
                    );
                    // unwrap safety for try_into() u8: next_end_read always <
                    // TRAILERS_END.len()
                    *body_state =
                        PS::ChunkedFinal(read, trailers_read, 0, next_end_read.try_into().unwrap());
                    Ok(TrailersEndParseState::NotEnd(next_parse_index))
                }
            }
            _ => panic!("wrong body state: {:?}", body_state),
        }
    }

    // Validates that the starting bytes of `buf` are the expected CRLF bytes.
    // Expects: buf that starts at the indices where CRLF should be for chunked bodies.
    // If need_lf_only, we will only check for LF, else we will check starting with CR.
    //
    // Returns Ok() if buf begins with expected bytes (CR, LF, or CRLF).
    // The inner bool returned is whether the whole CRLF sequence was completed.
    fn validate_crlf(
        body_state: &mut ParseState,
        buf: &[u8],
        need_lf_only: bool,
        for_trailer_end: bool,
    ) -> Result<bool> {
        let etype = if for_trailer_end {
            INVALID_TRAILER_END
        } else {
            INVALID_CHUNK
        };
        if need_lf_only {
            if buf.is_empty() {
                Ok(false)
            } else {
                let b = &buf[..1];
                if b == b"\n" {
                    // only LF left
                    Ok(true)
                } else {
                    *body_state = body_state.done(0);
                    Error::e_explain(
                        etype,
                        format!(
                            "Invalid chunked encoding: {} was not LF",
                            String::from_utf8_lossy(b).escape_default(),
                        ),
                    )
                }
            }
        } else {
            match buf.len() {
                0 => Ok(false),
                1 => {
                    let b = &buf[..1];
                    if b == b"\r" {
                        Ok(false)
                    } else {
                        *body_state = body_state.done(0);
                        Error::e_explain(
                            etype,
                            format!(
                                "Invalid chunked encoding: {} was not CR",
                                String::from_utf8_lossy(b).escape_default(),
                            ),
                        )
                    }
                }
                _ => {
                    let b = &buf[..2];
                    if b == b"\r\n" {
                        Ok(true)
                    } else {
                        *body_state = body_state.done(0);
                        Error::e_explain(
                            etype,
                            format!(
                                "Invalid chunked encoding: {} was not CRLF",
                                String::from_utf8_lossy(b).escape_default(),
                            ),
                        )
                    }
                }
            }
        }
    }
}

pub enum TrailersEndParseState {
    NotEnd(usize),   // start of bytes after CR or LF bytes
    Complete(usize), // index of message completion
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyMode {
    ToSelect,
    ContentLength(usize, usize), // total length to write, bytes already written
    ChunkedEncoding(usize),      //bytes written
    UntilClose(usize),           //bytes written
    Complete(usize),             //bytes written
}

type BM = BodyMode;

// ============================================================================
// Cancel-safe body writing types
// ============================================================================

impl BodyMode {
    /// Extract `(total, written)` from `ContentLength`, panicking on mismatch.
    fn expect_content_length(&self) -> (usize, usize) {
        match self {
            BodyMode::ContentLength(total, written) => (*total, *written),
            _ => panic!("wrong body mode: expected ContentLength, got {:?}", self),
        }
    }

    /// Extract `written` from `ChunkedEncoding`, panicking on mismatch.
    fn expect_chunked(&self) -> usize {
        match self {
            BodyMode::ChunkedEncoding(written) => *written,
            _ => panic!("wrong body mode: expected ChunkedEncoding, got {:?}", self),
        }
    }

    /// Extract `written` from `UntilClose`, panicking on mismatch.
    fn expect_until_close(&self) -> usize {
        match self {
            BodyMode::UntilClose(written) => *written,
            _ => panic!("wrong body mode: expected UntilClose, got {:?}", self),
        }
    }
}

/// Type alias for the chunked encoding buffer chain
type ChunkedBuf = bytes::buf::Chain<bytes::buf::Chain<Bytes, Bytes>, &'static [u8]>;

#[allow(dead_code)]
enum WriteBuf<C = ChunkedBuf> {
    /// Simple bytes buffer
    Simple(Bytes),
    /// Chained buffer for chunked encoding or other complex writes
    Chained(C),
}

// Implement Buf for WriteBuf to delegate to the inner buffer
impl<C: Buf> Buf for WriteBuf<C> {
    fn remaining(&self) -> usize {
        match self {
            WriteBuf::Simple(b) => b.remaining(),
            WriteBuf::Chained(c) => c.remaining(),
        }
    }

    fn chunk(&self) -> &[u8] {
        match self {
            WriteBuf::Simple(b) => b.chunk(),
            WriteBuf::Chained(c) => c.chunk(),
        }
    }

    fn advance(&mut self, cnt: usize) {
        match self {
            WriteBuf::Simple(b) => b.advance(cnt),
            WriteBuf::Chained(c) => c.advance(cnt),
        }
    }
}

#[allow(dead_code)]
enum WriteState {
    /// No write in progress
    Idle,
    /// Writing data (original size, bytes remaining to write)
    Writing(usize, WriteBuf<ChunkedBuf>),
    /// Flushing after write (original size to return)
    Flushing(usize),
    /// Write complete (bytes written in this task)
    Done(usize),
    /// Write timed out - cannot be reused
    TimedOut,
}

// Custom Debug implementation since we can't derive it with futures
impl std::fmt::Debug for WriteState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteState::Idle => write!(f, "Idle"),
            WriteState::Writing(size, _buf) => {
                write!(f, "Writing(size: {})", size)
            }
            WriteState::Flushing(size) => write!(f, "Flushing(size: {})", size),
            WriteState::Done(size) => write!(f, "Done(size: {})", size),
            WriteState::TimedOut => write!(f, "TimedOut"),
        }
    }
}

#[allow(dead_code)]
enum FinishWriteState {
    /// No finish task queued
    NotStarted,
    /// Finish queued but not started yet
    Idle,
    /// Writing last chunk marker (for chunked encoding)
    WritingLastChunk(WriteBuf),
    /// Flushing after writing last chunk
    Flushing,
    /// Finish complete
    Done,
}

// Custom Debug implementation since WriteBuf doesn't implement Debug
impl std::fmt::Debug for FinishWriteState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FinishWriteState::NotStarted => write!(f, "NotStarted"),
            FinishWriteState::Idle => write!(f, "Idle"),
            FinishWriteState::WritingLastChunk(_) => write!(f, "WritingLastChunk"),
            FinishWriteState::Flushing => write!(f, "Flushing"),
            FinishWriteState::Done => write!(f, "Done"),
        }
    }
}

/// Internal state for the cancel-safe body write state machine.
///
/// Tracks the pending body bytes, write progress
/// (idle → writing → flushing → done), and an optional timeout.
struct SendBodyState {
    /// Application bytes queued to be written
    pending_bytes: Option<Bytes>,
    /// Current write state for cancel-safe operations
    write_state: WriteState,
    /// Timeout duration for this write task
    timeout_duration: Option<std::time::Duration>,
    /// Timeout future (only created if write returns Pending)
    timeout_fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>>,
}

impl std::fmt::Debug for SendBodyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendBodyState")
            .field("pending_bytes", &self.pending_bytes)
            .field("write_state", &self.write_state)
            .field("timeout_duration", &self.timeout_duration)
            .field(
                "timeout_fut",
                &self.timeout_fut.as_ref().map(|_| "Some(Future)"),
            )
            .finish()
    }
}

impl SendBodyState {
    fn new() -> Self {
        SendBodyState {
            pending_bytes: None,
            write_state: WriteState::Idle,
            timeout_duration: None,
            timeout_fut: None,
        }
    }
}

/// Tracks how response body bytes are framed and written to the wire.
///
/// Supports both a legacy async API (`write_body` / `finish`) and a cancel-safe
/// task API that can be driven inside a `tokio::select!` loop without losing
/// write progress.
pub struct BodyWriter {
    pub body_mode: BodyMode,
    // Boxed to reduce inline size. Only used by the cancel-safe proxy task API.
    #[allow(dead_code)]
    send_body_state: Box<SendBodyState>,
    #[allow(dead_code)]
    send_finish_state: FinishWriteState,
}

impl Default for BodyWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BodyWriter {
    pub fn new() -> Self {
        BodyWriter {
            body_mode: BM::ToSelect,
            send_body_state: Box::new(SendBodyState::new()),
            send_finish_state: FinishWriteState::NotStarted,
        }
    }

    pub fn init_chunked(&mut self) {
        self.body_mode = BM::ChunkedEncoding(0);
    }

    pub fn init_close_delimited(&mut self) {
        self.body_mode = BM::UntilClose(0);
    }

    pub fn init_content_length(&mut self, cl: usize) {
        self.body_mode = BM::ContentLength(cl, 0);
    }

    pub fn convert_to_close_delimited(&mut self) {
        if matches!(self.body_mode, BodyMode::UntilClose(_)) {
            // nothing to do, already in close-delimited mode
            return;
        }

        // NOTE: any stream buffered data will be flushed in next
        // close-delimited write
        // reset body state to close-delimited (UntilClose)
        self.body_mode = BM::UntilClose(0);
    }

    // NOTE on buffering/flush stream when writing the body
    // Buffering writes can reduce the syscalls hence improves efficiency of the system
    // But it hurts real time communication
    // So we only allow buffering when the body size is known ahead, which is less likely
    // to be real time interaction

    pub async fn write_body<S>(&mut self, stream: &mut S, buf: &[u8]) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        trace!("Writing Body, size: {}", buf.len());
        match self.body_mode {
            BM::Complete(_) => Ok(None),
            BM::ContentLength(_, _) => self.do_write_body(stream, buf).await,
            BM::ChunkedEncoding(_) => self.do_write_chunked_body(stream, buf).await,
            BM::UntilClose(_) => self.do_write_until_close_body(stream, buf).await,
            BM::ToSelect => Ok(None), // Error here?
        }
    }

    pub fn finished(&self) -> bool {
        match self.body_mode {
            BM::Complete(_) => true,
            BM::ContentLength(total, written) => written >= total,
            _ => false,
        }
    }

    pub fn is_close_delimited(&self) -> bool {
        matches!(self.body_mode, BM::UntilClose(_))
    }

    async fn do_write_body<S>(&mut self, stream: &mut S, buf: &[u8]) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        match self.body_mode {
            BM::ContentLength(total, written) => {
                if written >= total {
                    // already written full length
                    return Ok(None);
                }
                let mut to_write = total - written;
                if to_write < buf.len() {
                    warn!("Trying to write data over content-length: {total}");
                } else {
                    to_write = buf.len();
                }
                let res = stream.write_all(&buf[..to_write]).await;
                match res {
                    Ok(()) => {
                        self.body_mode = BM::ContentLength(total, written + to_write);
                        if self.finished() {
                            stream.flush().await.or_err(WriteError, "flushing body")?;
                        }
                        Ok(Some(to_write))
                    }
                    Err(e) => Error::e_because(WriteError, "while writing body", e),
                }
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        }
    }

    async fn do_write_chunked_body<S>(
        &mut self,
        stream: &mut S,
        buf: &[u8],
    ) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        match self.body_mode {
            BM::ChunkedEncoding(written) => {
                let chunk_size = buf.len();

                let chuck_size_buf = format!("{:X}\r\n", chunk_size);
                let mut output_buf = Bytes::from(chuck_size_buf).chain(buf).chain(&b"\r\n"[..]);
                stream
                    .write_vec_all(&mut output_buf)
                    .await
                    .or_err(WriteError, "while writing body")?;
                stream.flush().await.or_err(WriteError, "flushing body")?;
                self.body_mode = BM::ChunkedEncoding(written + chunk_size);
                Ok(Some(chunk_size))
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        }
    }

    async fn do_write_until_close_body<S>(
        &mut self,
        stream: &mut S,
        buf: &[u8],
    ) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        match self.body_mode {
            BM::UntilClose(written) => {
                let res = stream.write_all(buf).await;
                match res {
                    Ok(()) => {
                        self.body_mode = BM::UntilClose(written + buf.len());
                        stream.flush().await.or_err(WriteError, "flushing body")?;
                        Ok(Some(buf.len()))
                    }
                    Err(e) => Error::e_because(WriteError, "while writing body", e),
                }
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        }
    }

    pub async fn finish<S>(&mut self, stream: &mut S) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        match self.body_mode {
            BM::Complete(_) => Ok(None),
            BM::ContentLength(_, _) => self.do_finish_body(stream),
            BM::ChunkedEncoding(_) => self.do_finish_chunked_body(stream).await,
            BM::UntilClose(_) => self.do_finish_until_close_body(stream),
            BM::ToSelect => Ok(None),
        }
    }

    fn do_finish_body<S>(&mut self, _stream: S) -> Result<Option<usize>> {
        match self.body_mode {
            BM::ContentLength(total, written) => {
                self.body_mode = BM::Complete(written);
                if written < total {
                    return Error::e_explain(
                        PREMATURE_BODY_END,
                        format!("Content-length: {total} bytes written: {written}"),
                    );
                }
                Ok(Some(written))
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        }
    }

    async fn do_finish_chunked_body<S>(&mut self, stream: &mut S) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        match self.body_mode {
            BM::ChunkedEncoding(written) => {
                let res = stream.write_all(&LAST_CHUNK[..]).await;
                self.body_mode = BM::Complete(written);
                match res {
                    Ok(()) => Ok(Some(written)),
                    Err(e) => Error::e_because(WriteError, "while writing body", e),
                }
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        }
    }

    fn do_finish_until_close_body<S>(&mut self, _stream: &mut S) -> Result<Option<usize>> {
        match self.body_mode {
            BM::UntilClose(written) => {
                self.body_mode = BM::Complete(written);
                Ok(Some(written))
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        }
    }

    // ========================================================================
    // Cancel-safe body task API
    // ========================================================================

    #[cfg(test)]
    pub fn has_pending_body_task(&self) -> bool {
        self.send_body_state.pending_bytes.is_some()
            || !matches!(
                self.send_body_state.write_state,
                WriteState::Idle | WriteState::Done(_) | WriteState::TimedOut
            )
    }

    /// Queue application bytes as a body write task with an optional timeout.
    /// This is a non-async function that just saves the bytes.
    /// Call `write_current_body_task()` to actually perform the write.
    ///
    /// The timeout, if provided, will be enforced internally across all
    /// write attempts, even if the write is cancelled and resumed via `tokio::select!`.
    #[allow(dead_code)]
    pub fn send_body_task(&mut self, bytes: Bytes, timeout: Option<std::time::Duration>) {
        assert!(
            matches!(
                self.send_body_state.write_state,
                WriteState::Idle | WriteState::Done(_)
            ),
            "send_body_task called while previous task is still in progress: {:?}",
            self.send_body_state.write_state
        );
        self.send_body_state.pending_bytes = Some(bytes);
        self.send_body_state.write_state = WriteState::Idle;
        self.send_body_state.timeout_duration = timeout;
        self.send_body_state.timeout_fut = None;
    }

    /// Writes the current queued body task to the stream.
    ///
    /// ## Cancel-safety
    ///
    /// This function can be safely used in a `tokio::select!` loop.
    /// Returns `Ok(Some(bytes_written))` when complete, `Ok(None)` if no bytes to write.
    #[allow(dead_code)]
    pub async fn write_current_body_task<S>(&mut self, stream: &mut S) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Use poll_fn to wrap our poll-based implementation
        std::future::poll_fn(|cx| self.poll_write_current_body_task(cx, Pin::new(stream))).await
    }

    /// Poll-based implementation for writing body tasks.
    /// This is the core implementation that maintains state across cancellations.
    fn poll_write_current_body_task<S>(
        &mut self,
        cx: &mut Context<'_>,
        stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Check if already timed out - don't allow reuse
        if matches!(self.send_body_state.write_state, WriteState::TimedOut) {
            return Poll::Ready(Error::e_explain(
                WriteTimedout,
                "write task previously timed out",
            ));
        }

        // Lazy timeout optimization: Poll write first, create timeout only if needed.
        //
        // This follows the pattern from `pingora_timeout::Timeout` to avoid allocating
        // and registering timeout futures when writes complete immediately (the common case).
        //
        // Fast path: Write completes → return immediately, no timeout future created
        // Slow path: Write blocks → lazily create timeout future and poll both

        // First, try the write operation
        // Dispatch to the appropriate body mode handler
        let result = match self.body_mode {
            BM::Complete(_) => Poll::Ready(Ok(None)),
            BM::ContentLength(_, _) => self.poll_write_content_length_body_task(cx, stream),
            BM::ChunkedEncoding(_) => self.poll_write_chunked_body_task(cx, stream),
            BM::UntilClose(_) => self.poll_write_until_close_body_task(cx, stream),
            BM::ToSelect => Poll::Ready(Ok(None)),
        };

        // If write completed immediately, return without ever creating/polling timeout
        if result.is_ready() {
            return result;
        }

        // Write returned Pending - lazily create and check timeout if duration is set
        if let Some(duration) = self.send_body_state.timeout_duration {
            let timeout = self.send_body_state.timeout_fut.get_or_insert_with(|| {
                Box::pin(pingora_timeout::sleep(duration))
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>
            });

            if timeout.as_mut().poll(cx).is_ready() {
                // Timeout fired! Mark state as timed out and clear the timeout future
                self.send_body_state.write_state = WriteState::TimedOut;
                self.send_body_state.timeout_fut = None;
                return Poll::Ready(Error::e_explain(
                    WriteTimedout,
                    "writing body task timed out",
                ));
            }
        }

        // Both write and timeout are pending
        Poll::Pending
    }

    // ========================================================================
    // Cancel-safe finish task API
    // ========================================================================

    #[cfg(test)]
    pub fn has_pending_finish_task(&self) -> bool {
        !matches!(
            self.send_finish_state,
            FinishWriteState::NotStarted | FinishWriteState::Done
        )
    }

    /// Queue a finish operation as a task.
    /// This is a non-async function that just marks the finish as pending.
    /// Call `write_current_finish_task()` to actually perform the finish.
    ///
    /// This API is stateful and cancel-safe - use it when you need to finish
    /// the body in a `tokio::select!` loop or other cancellable context.
    #[allow(dead_code)]
    pub fn send_finish_task(&mut self) {
        self.send_finish_state = FinishWriteState::Idle;
    }

    /// Async function that performs the current queued finish task on the stream.
    /// This function is cancel-safe and can be called in a `tokio::select!` loop.
    /// Returns `Ok(Some(bytes_written))` when complete, `Ok(None)` if already complete.
    ///
    /// This API is stateful - it tracks progress across cancellations and can be
    /// safely resumed after being dropped mid-execution.
    #[allow(dead_code)]
    pub async fn write_current_finish_task<S>(&mut self, stream: &mut S) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Use poll_fn to wrap our poll-based implementation
        std::future::poll_fn(|cx| self.poll_write_current_finish_task(cx, Pin::new(stream))).await
    }

    /// Poll-based implementation for finish tasks.
    /// This is the core implementation that maintains state across cancellations.
    fn poll_write_current_finish_task<S>(
        &mut self,
        cx: &mut Context<'_>,
        stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // If no finish queued, return None
        if matches!(
            self.send_finish_state,
            FinishWriteState::NotStarted | FinishWriteState::Done
        ) {
            return Poll::Ready(Ok(None));
        }

        // Route to body-mode-specific implementation
        match self.body_mode {
            BM::Complete(_) => Poll::Ready(Ok(None)),
            BM::ContentLength(_, _) => self.poll_finish_content_length_task(cx, stream),
            BM::ChunkedEncoding(_) => self.poll_finish_chunked_task(cx, stream),
            BM::UntilClose(_) => self.poll_finish_until_close_task(cx, stream),
            BM::ToSelect => Poll::Ready(Ok(None)),
        }
    }

    /// Finish content-length body - just validates and updates state.
    /// No I/O needed since body write tasks already flushed after the last write.
    fn poll_finish_content_length_task<S>(
        &mut self,
        _cx: &mut Context<'_>,
        _stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        let written = match self.body_mode {
            BM::ContentLength(total, w) => {
                if w < total {
                    self.send_finish_state = FinishWriteState::Done;
                    return Poll::Ready(Error::e_explain(
                        PREMATURE_BODY_END,
                        format!("Content-length: {total} bytes written: {w}"),
                    ));
                }
                w
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        };

        // All bytes written - just update state to Complete
        self.body_mode = BM::Complete(written);
        self.send_finish_state = FinishWriteState::Done;
        Poll::Ready(Ok(Some(written)))
    }

    /// Poll-based helper to finish chunked encoding body
    fn poll_finish_chunked_task<S>(
        &mut self,
        cx: &mut Context<'_>,
        mut stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        let written = match self.body_mode {
            BM::ChunkedEncoding(w) => w,
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        };

        loop {
            match &mut self.send_finish_state {
                FinishWriteState::Idle => {
                    // Start writing last chunk marker "0\r\n\r\n"
                    let buf = WriteBuf::Simple(Bytes::from_static(&LAST_CHUNK[..]));
                    self.send_finish_state = FinishWriteState::WritingLastChunk(buf);
                }
                FinishWriteState::WritingLastChunk(buf) => {
                    // Poll write_vec_all - write until all bytes are written
                    ready!(poll_write_vec_all_buf(cx, stream.as_mut(), buf))
                        .map_err(|e| Error::because(WriteError, "while writing last chunk", e))?;

                    // All bytes written, move to flushing state
                    self.send_finish_state = FinishWriteState::Flushing;
                }
                FinishWriteState::Flushing => {
                    // Poll flush
                    ready!(stream.as_mut().poll_flush(cx))
                        .map_err(|e| Error::because(WriteError, "flushing after last chunk", e))?;

                    // Flush complete! Update body_mode and mark done
                    self.body_mode = BM::Complete(written);
                    self.send_finish_state = FinishWriteState::Done;
                    return Poll::Ready(Ok(Some(written)));
                }
                FinishWriteState::Done => {
                    unreachable!(
                        "Done state should have been handled in poll_write_current_finish_task"
                    )
                }
                FinishWriteState::NotStarted => {
                    unreachable!("NotStarted state should have been handled in poll_write_current_finish_task")
                }
            }
        }
    }

    /// Finish until-close body - just updates state.
    /// No I/O needed since body write tasks already flushed after each write.
    fn poll_finish_until_close_task<S>(
        &mut self,
        _cx: &mut Context<'_>,
        _stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        let written = match self.body_mode {
            BM::UntilClose(w) => w,
            _ => panic!("wrong body mode: {:?}", self.body_mode),
        };

        // Just update state to Complete
        self.body_mode = BM::Complete(written);
        self.send_finish_state = FinishWriteState::Done;
        Poll::Ready(Ok(Some(written)))
    }

    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Internal helper to poll a body task that writes in content-length mode
    /// and flushes at end.
    fn poll_write_content_length_body_task<S>(
        &mut self,
        cx: &mut Context<'_>,
        mut stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Move to Writing state if we're Idle
        if matches!(self.send_body_state.write_state, WriteState::Idle) {
            if let Some(mut bytes) = self.send_body_state.pending_bytes.take() {
                let (total, written) = self.body_mode.expect_content_length();

                // Check if we've already written everything
                if written >= total {
                    self.send_body_state.write_state = WriteState::Done(0);
                    return Poll::Ready(Ok(None));
                }

                let original_size = bytes.len();
                let remaining = total - written;

                // Truncate bytes if they exceed content-length
                if original_size > remaining {
                    warn!(
                        "Trying to write {} bytes over content-length: {}, truncating to {}",
                        original_size, total, remaining
                    );
                    bytes.truncate(remaining);
                }

                let bytes_to_write = bytes.len();
                self.send_body_state.write_state =
                    WriteState::Writing(bytes_to_write, WriteBuf::Simple(bytes));
            } else {
                self.send_body_state.write_state = WriteState::Done(0);
                return Poll::Ready(Ok(None));
            }
        }

        // Handle Writing state - do the write, transition to Flushing or Done
        if let WriteState::Writing(size, ref mut buf) = &mut self.send_body_state.write_state {
            let bytes_written = *size;

            // Attempt write
            match ready!(poll_write_all_buf(cx, stream.as_mut(), buf)) {
                Ok(()) => {
                    // Write completed - update body_mode to track bytes written
                    let (total, written) = self.body_mode.expect_content_length();
                    self.body_mode = BM::ContentLength(total, written + bytes_written);

                    if written + bytes_written >= total {
                        // All content-length bytes written, flush needed
                        self.send_body_state.write_state = WriteState::Flushing(bytes_written);
                    } else {
                        // More bytes to come, no flush needed
                        self.send_body_state.write_state = WriteState::Done(bytes_written);
                    }
                }
                Err(e) => {
                    return Poll::Ready(Error::e_because(WriteError, "while writing body", e))
                }
            }
        }

        // Handle Flushing state - do the flush, transition to Done
        if let WriteState::Flushing(size) = self.send_body_state.write_state {
            let bytes_written = size;

            // Attempt flush
            match ready!(stream.poll_flush(cx)) {
                Ok(()) => {
                    // Flush completed - transition to Done
                    self.send_body_state.write_state = WriteState::Done(bytes_written);
                }
                Err(e) => return Poll::Ready(Error::e_because(WriteError, "flushing body", e)),
            }
        }

        // Return based on final state
        match self.send_body_state.write_state {
            WriteState::Done(size) => {
                self.send_body_state.timeout_fut = None;
                Poll::Ready(Ok(Some(size)))
            }
            WriteState::TimedOut => Poll::Ready(Error::e_explain(
                WriteTimedout,
                "write task previously timed out",
            )),
            WriteState::Writing(..) | WriteState::Flushing(..) => {
                unreachable!("Writing/Flushing states should have been handled above or returned Pending via ready!")
            }
            WriteState::Idle => {
                unreachable!("Idle state should have been handled in setup")
            }
        }
    }

    /// Poll-based implementation for chunked encoding mode
    fn poll_write_chunked_body_task<S>(
        &mut self,
        cx: &mut Context<'_>,
        mut stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Move to Writing state if we're Idle
        if matches!(self.send_body_state.write_state, WriteState::Idle) {
            if let Some(bytes) = self.send_body_state.pending_bytes.take() {
                let application_bytes_size = bytes.len();

                // Format the chunk: size\r\ndata\r\n
                let chunk_size_header = format!("{:X}\r\n", application_bytes_size);
                let output_buf = Bytes::from(chunk_size_header)
                    .chain(bytes)
                    .chain(&b"\r\n"[..]);

                // Store the chained buffer directly to avoid copying
                self.send_body_state.write_state =
                    WriteState::Writing(application_bytes_size, WriteBuf::Chained(output_buf));
            } else {
                self.send_body_state.write_state = WriteState::Done(0);
                return Poll::Ready(Ok(None));
            }
        }

        // Handle Writing state - do the write using vectored I/O, transition to Flushing
        if let WriteState::Writing(size, ref mut buf) = &mut self.send_body_state.write_state {
            let bytes_written = *size;

            // Attempt vectored write for chained buffer (chunk size + data + CRLF)
            match ready!(poll_write_vec_all_buf(cx, stream.as_mut(), buf)) {
                Ok(()) => {
                    // Write completed - update body_mode with application bytes (not wire bytes)
                    let written = self.body_mode.expect_chunked();
                    self.body_mode = BM::ChunkedEncoding(written + bytes_written);

                    // Chunked encoding always flushes
                    self.send_body_state.write_state = WriteState::Flushing(bytes_written);
                }
                Err(e) => {
                    return Poll::Ready(Error::e_because(WriteError, "while writing body", e))
                }
            }
        }

        // Handle Flushing state - do the flush, transition to Done
        if let WriteState::Flushing(size) = self.send_body_state.write_state {
            let bytes_written = size;

            // Attempt flush
            match ready!(stream.poll_flush(cx)) {
                Ok(()) => {
                    // Flush completed - transition to Done
                    self.send_body_state.write_state = WriteState::Done(bytes_written);
                }
                Err(e) => return Poll::Ready(Error::e_because(WriteError, "flushing body", e)),
            }
        }

        // Return based on final state
        match self.send_body_state.write_state {
            WriteState::Done(size) => {
                self.send_body_state.timeout_fut = None;
                Poll::Ready(Ok(Some(size)))
            }
            WriteState::TimedOut => Poll::Ready(Error::e_explain(
                WriteTimedout,
                "write task previously timed out",
            )),
            WriteState::Writing(..) | WriteState::Flushing(..) => {
                unreachable!("Writing/Flushing states should have been handled above or returned Pending via ready!")
            }
            WriteState::Idle => {
                unreachable!("Idle state should have been handled in setup")
            }
        }
    }

    /// Poll-based implementation for UntilClose (close-delimited) body mode
    fn poll_write_until_close_body_task<S>(
        &mut self,
        cx: &mut Context<'_>,
        mut stream: Pin<&mut S>,
    ) -> Poll<Result<Option<usize>>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        // Move to Writing state if we're Idle
        if matches!(self.send_body_state.write_state, WriteState::Idle) {
            if let Some(bytes) = self.send_body_state.pending_bytes.take() {
                let original_size = bytes.len();
                self.send_body_state.write_state =
                    WriteState::Writing(original_size, WriteBuf::Simple(bytes));
            } else {
                self.send_body_state.write_state = WriteState::Done(0);
                return Poll::Ready(Ok(None));
            }
        }

        // Handle Writing state - do the write, transition to Flushing
        if let WriteState::Writing(size, ref mut buf) = &mut self.send_body_state.write_state {
            let bytes_written = *size;

            // Attempt write
            match ready!(poll_write_all_buf(cx, stream.as_mut(), buf)) {
                Ok(()) => {
                    // Write completed - update body_mode to track bytes written
                    let written = self.body_mode.expect_until_close();
                    self.body_mode = BM::UntilClose(written + bytes_written);

                    // Close-delimited mode always flushes
                    self.send_body_state.write_state = WriteState::Flushing(bytes_written);
                }
                Err(e) => {
                    return Poll::Ready(Error::e_because(WriteError, "while writing body", e))
                }
            }
        }

        // Handle Flushing state - do the flush, transition to Done
        if let WriteState::Flushing(size) = self.send_body_state.write_state {
            let bytes_written = size;

            // Attempt flush
            match ready!(stream.poll_flush(cx)) {
                Ok(()) => {
                    // Flush completed - transition to Done
                    self.send_body_state.write_state = WriteState::Done(bytes_written);
                }
                Err(e) => return Poll::Ready(Error::e_because(WriteError, "flushing body", e)),
            }
        }

        // Return based on final state
        match self.send_body_state.write_state {
            WriteState::Done(size) => {
                self.send_body_state.timeout_fut = None;
                Poll::Ready(Ok(Some(size)))
            }
            WriteState::TimedOut => Poll::Ready(Error::e_explain(
                WriteTimedout,
                "write task previously timed out",
            )),
            WriteState::Writing(..) | WriteState::Flushing(..) => {
                unreachable!("Writing/Flushing states should have been handled above or returned Pending via ready!")
            }
            WriteState::Idle => {
                unreachable!("Idle state should have been handled in setup")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io::Builder;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn read_with_body_content_length() {
        init_log();
        let input = b"abc";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 3));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(input, body_reader.get_body(&res));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_content_length_2() {
        init_log();
        let input1 = b"a";
        let input2 = b"bc";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(input2, body_reader.get_body(&res));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_content_length_less() {
        init_log();
        let input1 = b"a";
        let input2 = b""; // simulating close
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(&ConnectionClosed, res.etype());
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_content_length_more() {
        init_log();
        let input1 = b"a";
        let input2 = b"bcd";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(&input2[0..2], body_reader.get_body(&res));
        // read remaining data
        body_reader.init_content_length(1, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(&input2[2..], body_reader.get_body(&res));
    }

    #[tokio::test]
    async fn read_with_body_content_length_overread() {
        init_log();
        let input1 = b"a";
        let input2 = b"bcd";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(true);
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(&input2[0..2], body_reader.get_body(&res));
        assert_eq!(body_reader.get_body_overread(), Some(&b"d"[..]));
    }

    #[tokio::test]
    async fn read_with_body_content_length_rewind() {
        init_log();
        let rewind = b"ab";
        let input = b"c";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_content_length(3, rewind);
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::Partial(2, 1));
        assert_eq!(rewind, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(input, body_reader.get_body(&res));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_http10() {
        init_log();
        let input1 = b"a";
        let input2 = b""; // simulating close
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_close_delimited(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::UntilClose(1));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_http10_rewind() {
        init_log();
        let rewind = b"ab";
        let input1 = b"c";
        let input2 = b""; // simulating close
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_close_delimited(rewind);
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::UntilClose(2));
        assert_eq!(rewind, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::UntilClose(3));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_zero_chunk() {
        init_log();
        let input = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_zero_chunk_malformed() {
        init_log();
        let input = b"0\r\nr\n";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(0, 0, 2, 2));

        // \n without leading \r
        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_TRAILER_END);
        assert_eq!(body_reader.body_state, ParseState::Done(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_zero_chunk_split() {
        init_log();
        let input1 = b"0\r\n";
        let input2 = b"\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(0, 0, 0, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_zero_chunk_split_head() {
        init_log();
        let input1 = b"0\r";
        let input2 = b"\n";
        let input3 = b"\r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(0, 0, 0, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_zero_chunk_split_head_2() {
        init_log();
        let input1 = b"0";
        let input2 = b"\r\n";
        let input3 = b"\r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 1, 1));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(0, 0, 0, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_zero_chunk_split_head_3() {
        init_log();
        let input1 = b"0\r";
        let input2 = b"\n";
        let input3 = b"\r";
        let input4 = b"\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .read(&input4[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(0, 0, 0, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(0, 0, 0, 3));

        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_chunk_ext() {
        init_log();
        let input = b"0;aaaa\r\n\r\n";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_chunk_ext_oversize() {
        init_log();
        let chunk_size = b"0;";
        let ext1 = [b'a'; 1024 * 5];
        let ext2 = [b'a'; 1024 * 3];
        let mut mock_io = Builder::new()
            .read(&chunk_size[..])
            .read(&ext1[..])
            .read(&ext2[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        // read chunk-size, chunk incomplete
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, Some(BufRef::new(0, 0)));
        // read ext1, chunk incomplete
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, Some(BufRef::new(0, 0)));
        // read ext2, now oversized
        let res = body_reader.read_body(&mut mock_io).await;
        assert!(res.is_err());
        assert_eq!(body_reader.body_state, ParseState::Done(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_1_chunk() {
        init_log();
        let input1 = b"1\r\na\r\n";
        let input2 = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_malformed() {
        init_log();
        let input1 = b"1\r\na\rn";
        let mut mock_io = Builder::new().read(&input1[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");

        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_CHUNK);
        assert_eq!(body_reader.body_state, ParseState::Done(0));

        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_partial_end() {
        init_log();
        let input1 = b"1\r\na\r";
        let input2 = b"\n0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 0, 1));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 1, 6, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_partial_end_1() {
        init_log();
        let input1 = b"3\r\n";
        let input2 = b"abc\r";
        let input3 = b"\n0\r\n\r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 0));
        assert_eq!(b"", body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 0, 5));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 3));
        assert_eq!(&input2[0..3], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 0, 0, 1));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_partial_end_2() {
        init_log();
        let input1 = b"3\r\n";
        let input2 = b"abc";
        let input3 = b"\r\n0\r\n\r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 0));
        assert_eq!(b"", body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 0, 5));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 3));
        assert_eq!(&input2[0..3], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 0, 0, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_incomplete() {
        init_log();
        let input1 = b"1\r\na\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 0, 0));
        let res = body_reader.read_body(&mut mock_io).await;
        assert!(res.is_err());
        assert_eq!(body_reader.body_state, ParseState::Done(1));
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_partial_end_malformed() {
        init_log();
        let input1 = b"1\r\na\r";
        let input2 = b"n0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 0, 1));
        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_CHUNK);
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_rewind() {
        init_log();
        let rewind = b"1\r\nx\r\n";
        let input1 = b"1\r\na\r\n";
        let input2 = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(rewind);
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&rewind[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(2, 0, 0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(2));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_multi_chunk() {
        init_log();
        let input1 = b"1\r\na\r\n2\r\nbc\r\n";
        let input2 = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 13, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(9, 2));
        assert_eq!(&input1[9..11], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 0, 0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_multi_chunk_malformed() {
        init_log();
        let input1 = b"1\r\na\r\n2\r\nbcr\n";
        let mut mock_io = Builder::new().read(&input1[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");

        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 13, 0));

        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_CHUNK);
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);

        let input1 = b"1\r\nar\n2\r\nbc\rn";
        let mut mock_io = Builder::new().read(&input1[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");

        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_CHUNK);
        assert_eq!(body_reader.body_state, ParseState::Done(0));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_partial_chunk() {
        init_log();
        let input1 = b"3\r\na";
        let input2 = b"bc\r\n0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 0, 4));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(&input2[0..2], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 4, 9, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_partial_chunk_end() {
        init_log();
        let input1 = b"3\r\nabc";
        let input2 = b"\r\n0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 3));
        assert_eq!(&input1[3..6], body_reader.get_body(&res));
        // \r\n (2 bytes) left to read from IO
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 0, 0, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(&input2[0..0], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 2, 7, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_partial_head_chunk() {
        init_log();
        let input1 = b"1\r";
        let input2 = b"\na\r\n0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));
        let _res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn read_with_body_partial_head_chunk_incomplete() {
        init_log();
        let input1 = b"1\r";
        let mut mock_io = Builder::new().read(&input1[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));
        let res = body_reader.read_body(&mut mock_io).await;
        assert!(res.is_err());
        assert_eq!(body_reader.body_state, ParseState::Done(0));
    }

    #[tokio::test]
    async fn read_with_body_partial_head_terminal_crlf() {
        init_log();
        let input1 = b"1\r";
        let input2 = b"\na\r\n0\r\n\r";
        let input3 = b"\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1)); // input1 concat input2
        assert_eq!(&input2[1..2], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 10, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0)); // only part of terminal crlf, one more byte to read
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 1, 2));
        // TODO: can optimize this to avoid the second read_body call
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 0, 3));

        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_partial_head_terminal_crlf_2() {
        init_log();
        let input1 = b"1\r";
        let input2 = b"\na\r\n0\r";
        let input3 = b"\n\r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1)); // input1 concat input2
        assert_eq!(&input2[1..2], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 8, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0)); // only part of terminal crlf, one more byte to read
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 8, 2));
        // optimized to go right to complete state
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_partial_head_terminal_crlf_3() {
        init_log();
        let input1 = b"1\r\na\r\n0";
        let input2 = b"\r";
        let input3 = b"\n";
        let input4 = b"\r";
        let input5 = b"\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .read(&input4[..])
            .read(&input5[..])
            .build();

        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 7, 0));
        // to 0
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 7, 1));
        // \r
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 2, 2));
        // \n
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 0, 2));
        // \r
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 0, 3));
        // \n
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_partial_head_terminal_crlf_malformed() {
        init_log();
        let input1 = b"1\r";
        let input2 = b"\na\r\n0\r\nr";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");

        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));

        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1)); // input1 concat input2
        assert_eq!(&input2[1..2], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 10, 0));

        // TODO: may be able to optimize this extra read_body out
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 1, 2));
        // "r" is interpreted as a hanging trailer
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 3, 0, 0));

        let res = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(&ConnectionClosed, res.etype());
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_partial_head_terminal_crlf_overread() {
        init_log();
        let input1 = b"1\r";
        let input2 = b"\na\r\n0\r\n\r";
        let input3 = b"\nabcd";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(0, 0, 2, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1)); // input1 concat input2
        assert_eq!(&input2[1..2], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 10, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0)); // read only part of terminal crlf
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 1, 2));
        // TODO: can optimize this to avoid the second read_body call
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 0, 3));

        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), Some(&b"abcd"[..]));
    }

    #[tokio::test]
    async fn read_with_body_multi_chunk_overread() {
        init_log();
        let input1 = b"1\r\na\r\n2\r\nbc\r\n";
        let input2 = b"0\r\n\r\nabc";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 13, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(9, 2));
        assert_eq!(&input1[9..11], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 0, 0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), Some(&b"abc"[..]));
    }

    #[tokio::test]
    async fn read_with_body_trailers() {
        init_log();
        let input1 = b"1\r\na\r\n2\r\nbc\r\n";
        let input2 = b"0\r\nabc: hi";
        let input3 = b"\r\ndef: bye\r";
        let input4 = b"\nghi: more\r\n";
        let input5 = b"\r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .read(&input4[..])
            .read(&input5[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 13, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(9, 2));
        assert_eq!(&input1[9..11], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(3, 0, 0, 0));
        // abc: hi
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(3, 0, 7, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            // NOTE: 0 chunk-size CRLF counted in trailer size too
            ParseState::ChunkedFinal(3, 9, 0, 0)
        );
        // def: bye
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            ParseState::ChunkedFinal(3, 19, 0, 1)
        );
        // ghi: more
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            ParseState::ChunkedFinal(3, 30, 0, 2)
        );

        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_trailers_2() {
        init_log();
        let input1 = b"1\r\na\r\n0\r";
        let input2 = b"\nabc: hi\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 8, 0));
        // 0 \r
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 8, 2));
        // \n TODO: optimize this call out
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            ParseState::ChunkedFinal(1, 0, 11, 2)
        );
        // abc: hi with end in same read
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_trailers_3() {
        init_log();
        let input1 = b"1\r\na\r\n0\r";
        let input2 = b"\nabc: hi";
        let input3 = b"\r\n\r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 8, 0));
        // 0 \r
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 8, 2));
        // \n TODO: optimize this call out
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 7, 2));
        // abc: hi
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            // NOTE: 0 chunk-size CRLF counted in trailer size too
            ParseState::ChunkedFinal(1, 9, 0, 0)
        );
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_trailers_4() {
        init_log();
        let input1 = b"1\r\na\r\n0\r";
        let input2 = b"\nabc: hi\r\n\r";
        let input3 = b"\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 8, 0));
        // 0 \r
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 8, 2));
        // \n TODO: optimize this call out
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            ParseState::ChunkedFinal(1, 0, 10, 2)
        );
        // abc: hi
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            // NOTE: 0 chunk-size CRLF counted in trailer size too
            ParseState::ChunkedFinal(1, 9, 0, 3)
        );
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_trailers_malformed() {
        init_log();
        let input1 = b"1\r\na\r\n0\r";
        let input2 = b"\nabc: hi\rn";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 8, 0));
        // 0 \r
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 8, 2));
        // abc: hi to \rn
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 9, 2));
        // \rn not valid
        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_TRAILER_END);
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_trailers_malformed_2() {
        init_log();
        let input1 = b"1\r\na\r\n0\r";
        let input2 = b"\nabc: hi\r\n";
        // no end
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 8, 0));
        // 0 \r
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 8, 2));
        // abc: hi to \r\n
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 9, 2));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 9, 0, 2));
        // EOF
        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), ConnectionClosed);
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_trailers_malformed_3() {
        init_log();
        let input1 = b"1\r\na\r\n0\r\n";
        let input2 = b"abc: hi\r\n";
        let input3 = b"r\n";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&input3[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 9, 0));
        // 0 \r\n
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 0, 2));
        // abc: hi
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 9, 0, 2));
        // r\n not valid
        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_TRAILER_END);
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn read_with_body_trailers_overflow() {
        init_log();
        let input1 = b"1\r\na\r\n0\r\n";
        let input2 = b"abc: ";
        let trailer1 = [b'a'; 1024 * 60];
        let trailer2 = [b'a'; 1024 * 5];
        let input3 = b"defghi: ";
        let mut mock_io = Builder::new()
            .read(&input1[..])
            .read(&input2[..])
            .read(&trailer1[..])
            .read(&CRLF[..])
            .read(&input3[..])
            .read(&trailer2[..])
            .build();
        let mut body_reader = BodyReader::new(false);
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 9, 0));
        // 0 \r\n
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 0, 0, 2));
        // abc:
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(body_reader.body_state, ParseState::ChunkedFinal(1, 7, 0, 0));
        // aaa...
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            ParseState::ChunkedFinal(1, 1024 * 60 + 7, 0, 0)
        );
        // CRLF
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            ParseState::ChunkedFinal(1, 1024 * 60 + 7, 0, 2)
        );
        // defghi:
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 0));
        assert_eq!(
            body_reader.body_state,
            ParseState::ChunkedFinal(1, 1024 * 60 + 17, 0, 0)
        );
        // overflow
        let e = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(*e.etype(), INVALID_TRAILER_END);
        assert_eq!(body_reader.body_state, ParseState::Done(1));
        assert_eq!(body_reader.get_body_overread(), None);
    }

    #[tokio::test]
    async fn write_body_cl() {
        init_log();
        let output = b"a";
        let mut mock_io = Builder::new().write(&output[..]).build();
        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(1);
        assert_eq!(body_writer.body_mode, BodyMode::ContentLength(1, 0));
        let res = body_writer
            .write_body(&mut mock_io, &output[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::ContentLength(1, 1));
        // write again, over the limit
        let res = body_writer
            .write_body(&mut mock_io, &output[..])
            .await
            .unwrap();
        assert_eq!(res, None);
        assert_eq!(body_writer.body_mode, BodyMode::ContentLength(1, 1));
        let res = body_writer.finish(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::Complete(1));
    }

    #[tokio::test]
    async fn write_body_chunked() {
        init_log();
        let data = b"abcdefghij";
        let output = b"A\r\nabcdefghij\r\n";
        let mut mock_io = Builder::new()
            .write(&output[..])
            .write(&output[..])
            .write(&LAST_CHUNK[..])
            .build();
        let mut body_writer = BodyWriter::new();
        body_writer.init_chunked();
        assert_eq!(body_writer.body_mode, BodyMode::ChunkedEncoding(0));
        let res = body_writer
            .write_body(&mut mock_io, &data[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, data.len());
        assert_eq!(body_writer.body_mode, BodyMode::ChunkedEncoding(data.len()));
        let res = body_writer
            .write_body(&mut mock_io, &data[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, data.len());
        assert_eq!(
            body_writer.body_mode,
            BodyMode::ChunkedEncoding(data.len() * 2)
        );
        let res = body_writer.finish(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, data.len() * 2);
        assert_eq!(body_writer.body_mode, BodyMode::Complete(data.len() * 2));
    }

    #[tokio::test]
    async fn write_body_until_close() {
        init_log();
        let data = b"a";
        let mut mock_io = Builder::new().write(&data[..]).write(&data[..]).build();
        let mut body_writer = BodyWriter::new();
        body_writer.init_close_delimited();
        assert_eq!(body_writer.body_mode, BodyMode::UntilClose(0));
        let res = body_writer
            .write_body(&mut mock_io, &data[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::UntilClose(1));
        let res = body_writer
            .write_body(&mut mock_io, &data[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::UntilClose(2));
        let res = body_writer.finish(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, 2);
        assert_eq!(body_writer.body_mode, BodyMode::Complete(2));
    }
}

#[cfg(test)]
mod test_body_task_api {
    use super::*;
    use tokio_test::io::Builder;

    // Cancel-safety tests use tokio::select! to race a short sleep against a mock
    // I/O wait, simulating cancellation. We use #[tokio::test(start_paused = true)]
    // on these tests so that tokio auto-advances time deterministically rather than
    // relying on wall-clock timing.

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn test_has_pending_body_task() {
        init_log();
        let data = b"test data";

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());

        // Initially should have no pending task
        assert!(!body_writer.has_pending_body_task());

        // After queuing bytes, should have pending task
        body_writer.send_body_task(Bytes::from_static(data), None);
        assert!(body_writer.has_pending_body_task());
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_safe_content_length_write() {
        init_log();
        let data = b"Hello, World!";

        // Create a mock stream that will block to allow cancellation
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(100))
            .write(data)
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());

        // Queue the bytes to write
        body_writer.send_body_task(Bytes::from_static(data), None);

        // Use tokio::select! loop - keep looping until write completes
        let mut cancel_count = 0;
        let mut total_bytes_written = 0;

        loop {
            // Break if no pending writes
            if !body_writer.has_pending_body_task() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    // Timeout fires first, cancelling the write
                    cancel_count += 1;
                }
                result = body_writer.write_current_body_task(&mut mock_io) => {
                    // Write completed
                    assert!(result.is_ok(), "Write should succeed");
                    if let Ok(Some(n)) = result {
                        total_bytes_written += n;
                    }
                }
            }
        }

        assert!(
            cancel_count > 0,
            "At least one cancellation should have occurred"
        );
        assert_eq!(
            total_bytes_written,
            data.len(),
            "Should have written all application bytes"
        );
        assert_eq!(
            body_writer.body_mode,
            BodyMode::ContentLength(data.len(), data.len())
        );

        // Now test finish() in a select loop as well
        let mut mock_io_finish = Builder::new().build();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                    // Allow cancellation attempts
                }
                result = body_writer.finish(&mut mock_io_finish) => {
                    assert!(result.is_ok());
                    break;
                }
            }
        }

        assert_eq!(body_writer.body_mode, BodyMode::Complete(data.len()));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_safe_chunked_write() {
        init_log();
        let data = b"abcdefghij";
        let expected_output = b"A\r\nabcdefghij\r\n";

        // Mock stream that blocks to allow cancellation
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(100))
            .write(expected_output)
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_chunked();

        // Queue bytes
        body_writer.send_body_task(Bytes::from_static(data), None);

        // Use select loop - keep looping until write completes
        let mut cancel_count = 0;
        let mut total_bytes_written = 0;

        loop {
            if !body_writer.has_pending_body_task() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    cancel_count += 1;
                }
                result = body_writer.write_current_body_task(&mut mock_io) => {
                    assert!(result.is_ok());
                    if let Ok(Some(n)) = result {
                        total_bytes_written += n;
                    }
                }
            }
        }

        assert!(cancel_count > 0, "Should have cancelled at least once");
        assert_eq!(
            total_bytes_written,
            data.len(),
            "Should have written all application bytes"
        );
        assert_eq!(body_writer.body_mode, BodyMode::ChunkedEncoding(data.len()));

        // Test finish() with select loop - must write terminating chunk
        let mut mock_io_finish = Builder::new()
            .wait(std::time::Duration::from_millis(50))
            .write(&LAST_CHUNK[..]) // Expect 0\r\n\r\n
            .build();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                result = body_writer.finish(&mut mock_io_finish) => {
                    assert!(result.is_ok());
                    break;
                }
            }
        }

        assert_eq!(body_writer.body_mode, BodyMode::Complete(data.len()));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_safe_until_close_write() {
        init_log();
        let data = b"test data";

        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(100))
            .write(data)
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_close_delimited();

        body_writer.send_body_task(Bytes::from_static(data), None);

        // Use select loop - keep looping until write completes
        let mut cancel_count = 0;
        let mut total_bytes_written = 0;

        loop {
            if !body_writer.has_pending_body_task() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    cancel_count += 1;
                }
                result = body_writer.write_current_body_task(&mut mock_io) => {
                    assert!(result.is_ok());
                    if let Ok(Some(n)) = result {
                        total_bytes_written += n;
                    }
                }
            }
        }

        assert!(cancel_count > 0);
        assert_eq!(
            total_bytes_written,
            data.len(),
            "Should have written all application bytes"
        );

        // Test finish() with select loop
        let mut mock_io_finish = Builder::new().build();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                result = body_writer.finish(&mut mock_io_finish) => {
                    assert!(result.is_ok());
                    break;
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_safe_multiple_cancellations() {
        init_log();
        let data = b"Long test data that requires multiple writes";

        // Create a mock that blocks multiple times
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(50))
            .write(&data[..15])
            .wait(std::time::Duration::from_millis(50))
            .write(&data[15..30])
            .wait(std::time::Duration::from_millis(50))
            .write(&data[30..])
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());
        body_writer.send_body_task(Bytes::from_static(data), None);

        // Loop until write completes, allowing cancellations
        let mut cancel_count = 0;
        let mut total_bytes_written = 0;

        loop {
            if !body_writer.has_pending_body_task() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                    cancel_count += 1;
                }
                result = body_writer.write_current_body_task(&mut mock_io) => {
                    assert!(result.is_ok());
                    if let Ok(Some(n)) = result {
                        total_bytes_written += n;
                    }
                }
            }
        }

        assert!(cancel_count >= 2, "Should have multiple cancellations");
        assert_eq!(
            total_bytes_written,
            data.len(),
            "Should have written all application bytes"
        );

        // Test finish with select loop
        let mut mock_io_finish = Builder::new().build();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                result = body_writer.finish(&mut mock_io_finish) => {
                    assert!(result.is_ok());
                    break;
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_safe_partial_writes() {
        init_log();
        let data = b"12345678901234567890"; // 20 bytes

        // Simulate partial writes with blocking
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(50))
            .write(&data[..7])
            .wait(std::time::Duration::from_millis(50))
            .write(&data[7..14])
            .wait(std::time::Duration::from_millis(50))
            .write(&data[14..])
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());
        body_writer.send_body_task(Bytes::from_static(data), None);

        let mut cancel_count = 0;
        let mut total_bytes_written = 0;

        loop {
            if !body_writer.has_pending_body_task() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    cancel_count += 1;
                }
                result = body_writer.write_current_body_task(&mut mock_io) => {
                    assert!(result.is_ok());
                    if let Ok(Some(n)) = result {
                        total_bytes_written += n;
                    }
                }
            }
        }

        assert!(cancel_count > 0);
        assert_eq!(
            total_bytes_written,
            data.len(),
            "Should have written all application bytes"
        );

        // Test finish in select loop
        let mut mock_io_finish = Builder::new()
            .wait(std::time::Duration::from_millis(30))
            .build();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                result = body_writer.finish(&mut mock_io_finish) => {
                    assert!(result.is_ok(), "Finish should succeed after cancel-safe writes");
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn test_task_write_timeout() {
        init_log();
        let data = b"test data";

        // Create a mock that blocks forever
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_secs(1000))
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());

        // Queue the task with a timeout
        body_writer.send_body_task(
            Bytes::from_static(data),
            Some(std::time::Duration::from_millis(50)),
        );

        // The write should timeout
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(result.is_err(), "Write should timeout");

        // Check that it's a timeout error
        if let Err(e) = result {
            assert_eq!(e.etype(), &WriteTimedout);
        }
    }

    // Even if the user's select! cancels the write, the internal timeout
    // should continue counting across cancellations.
    #[tokio::test]
    async fn test_task_timeout_persists_across_cancellations() {
        init_log();
        let data = b"test data";

        // Create a mock that blocks for a while
        // Since timeout is 100ms and this waits 200ms, the write should never happen
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(200))
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());

        // Queue the task with a 100ms timeout
        body_writer.send_body_task(
            Bytes::from_static(data),
            Some(std::time::Duration::from_millis(100)),
        );

        let mut attempts = 0;
        let mut timedout = false;

        // Try to write in a loop, but cancel early each time
        // The timeout should still fire even though we're cancelling
        loop {
            if !body_writer.has_pending_body_task() {
                break;
            }

            attempts += 1;

            tokio::select! {
                // Cancel after just 10ms each time
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    // Cancelled by our select, continue looping
                    continue;
                }
                result = body_writer.write_current_body_task(&mut mock_io) => {
                    match result {
                        Ok(_) => {
                            // Write succeeded before timeout
                            break;
                        }
                        Err(e) if e.etype() == &WriteTimedout => {
                            // Timeout fired!
                            timedout = true;
                            break;
                        }
                        Err(e) => {
                            panic!("Unexpected error: {:?}", e);
                        }
                    }
                }
            }
        }

        assert!(timedout, "Timeout should have fired despite cancellations");
        assert!(
            attempts >= 5,
            "Should have had multiple attempts before timeout"
        );
    }

    #[tokio::test]
    async fn test_task_write_succeeds_within_timeout() {
        init_log();
        let data = b"Hello, World!";

        // Create a mock that completes quickly
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(20))
            .write(data)
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());

        // Queue with a generous timeout
        body_writer.send_body_task(
            Bytes::from_static(data),
            Some(std::time::Duration::from_millis(500)),
        );

        // Write should succeed
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(result.is_ok(), "Write should succeed: {:?}", result);
        assert_eq!(result.unwrap(), Some(data.len()));
    }

    #[tokio::test]
    async fn test_task_write_no_timeout() {
        init_log();
        let data = b"test data";

        // Create a mock that takes a bit of time but eventually succeeds
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(100))
            .write(data)
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());

        // Queue without timeout
        body_writer.send_body_task(Bytes::from_static(data), None);

        // Write should eventually succeed
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(result.is_ok(), "Write should succeed without timeout");
        assert_eq!(result.unwrap(), Some(data.len()));
    }

    #[tokio::test]
    async fn test_task_chunked_write_timeout() {
        init_log();
        let data = b"chunked data";

        // Create a mock that blocks
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_secs(1000))
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_chunked();

        // Queue with short timeout
        body_writer.send_body_task(
            Bytes::from_static(data),
            Some(std::time::Duration::from_millis(50)),
        );

        // Should timeout
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.etype(), &WriteTimedout);
        }
    }

    #[tokio::test]
    async fn test_task_timeout_reset_on_new_task() {
        init_log();
        let data1 = b"first";
        let data2 = b"second";

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data1.len() + data2.len());

        // Queue first task with short timeout
        body_writer.send_body_task(
            Bytes::from_static(data1),
            Some(std::time::Duration::from_millis(50)),
        );

        // Wait a bit but don't let it timeout yet
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Queue a new task with a longer timeout
        // This should reset/replace the timeout
        body_writer.send_body_task(
            Bytes::from_static(data2),
            Some(std::time::Duration::from_millis(500)),
        );

        // Create a mock that takes some time
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(100))
            .write(data2)
            .build();

        // The second write should succeed with its own timeout
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(
            result.is_ok(),
            "Second task should succeed with new timeout"
        );
    }

    #[tokio::test]
    async fn test_task_timeout_with_partial_writes() {
        init_log();
        let data1 = b"first";
        let data2 = b"second";
        let data3 = b"third";

        // Mock that writes data1 quickly, data2 with delay, data3 blocks forever
        let mut mock_io = Builder::new()
            .wait(std::time::Duration::from_millis(10))
            .write(data1)
            .wait(std::time::Duration::from_millis(40))
            .write(data2)
            .wait(std::time::Duration::from_secs(1000))
            .build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data1.len() + data2.len() + data3.len());

        let mut total_written = 0;

        // First write - should succeed within timeout
        body_writer.send_body_task(
            Bytes::from_static(data1),
            Some(std::time::Duration::from_millis(100)),
        );
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(result.is_ok());
        total_written += result.unwrap().unwrap();

        // Second write - should succeed within timeout
        body_writer.send_body_task(
            Bytes::from_static(data2),
            Some(std::time::Duration::from_millis(100)),
        );
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(result.is_ok());
        total_written += result.unwrap().unwrap();

        // Third write - should timeout
        body_writer.send_body_task(
            Bytes::from_static(data3),
            Some(std::time::Duration::from_millis(50)),
        );
        let result = body_writer.write_current_body_task(&mut mock_io).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().etype(), &WriteTimedout);

        // We should have written data1 and data2 but not data3
        assert_eq!(total_written, data1.len() + data2.len());
        assert!(
            total_written < data1.len() + data2.len() + data3.len(),
            "Should not have written all data"
        );
    }

    // Cancel-safe finish task for chunked encoding: send_finish_task() queues
    // the terminating chunk, write_current_finish_task() writes it and can be
    // cancelled and resumed in a select! loop.
    #[tokio::test(start_paused = true)]
    async fn cancel_safe_finish_task_chunked() {
        init_log();

        let data = Bytes::from("hello");
        let expected_chunk = b"5\r\nhello\r\n";

        let mut mock_io = Builder::new().write(expected_chunk).build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_chunked();

        // Write body data via task API
        body_writer.send_body_task(data, None);
        body_writer
            .write_current_body_task(&mut mock_io)
            .await
            .unwrap();

        // Queue the finish task
        body_writer.send_finish_task();
        assert!(body_writer.has_pending_finish_task());

        // Write the finish in a select! loop with cancellations
        let mut mock_io_finish = Builder::new()
            .wait(std::time::Duration::from_millis(100))
            .write(b"0\r\n\r\n")
            .build();

        let mut cancel_count = 0;

        loop {
            if !body_writer.has_pending_finish_task() {
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    cancel_count += 1;
                }
                result = body_writer.write_current_finish_task(&mut mock_io_finish) => {
                    assert!(result.is_ok());
                    break;
                }
            }
        }

        assert!(cancel_count > 0, "Should have cancelled at least once");
        assert!(matches!(body_writer.body_mode, BodyMode::Complete(_)));
    }

    // Finish task for content-length is a no-op (no terminating chunk needed),
    // but it should still transition body_mode to Complete.
    #[tokio::test]
    async fn finish_task_content_length() {
        init_log();

        let data = b"hello";
        let mut mock_io = Builder::new().write(data).build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(data.len());

        body_writer.send_body_task(Bytes::from_static(data), None);
        body_writer
            .write_current_body_task(&mut mock_io)
            .await
            .unwrap();

        body_writer.send_finish_task();
        let mut mock_io_finish = Builder::new().build();
        let result = body_writer
            .write_current_finish_task(&mut mock_io_finish)
            .await;
        assert!(result.is_ok());
        assert!(matches!(body_writer.body_mode, BodyMode::Complete(_)));
    }

    // Verifies that body_mode byte tracking is correct when writing
    // content-length body in multiple chunks. Each intermediate chunk
    // does not trigger a flush; the body_mode must still accumulate
    // bytes correctly so that finish_task succeeds.
    #[tokio::test]
    async fn content_length_body_mode_tracks_across_chunks() {
        init_log();

        let chunk1 = b"Hello";
        let chunk2 = b", World!";
        let total_len = chunk1.len() + chunk2.len(); // 13

        // Mock expects both writes; the final write triggers a flush internally
        let mut mock_io = Builder::new().write(chunk1).write(chunk2).build();

        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(total_len);

        // Write first chunk (intermediate, no flush expected)
        body_writer.send_body_task(Bytes::from_static(chunk1), None);
        let result = body_writer
            .write_current_body_task(&mut mock_io)
            .await
            .unwrap();
        assert_eq!(result, Some(chunk1.len()));
        assert!(
            !body_writer.finished(),
            "Should not be finished after first chunk"
        );

        // Verify body_mode tracks the bytes from the first chunk
        assert!(
            matches!(body_writer.body_mode, BodyMode::ContentLength(total, written)
                if total == total_len && written == chunk1.len()),
            "body_mode should reflect bytes written so far, got: {:?}",
            body_writer.body_mode
        );

        // Write second chunk (final, completes content-length)
        body_writer.send_body_task(Bytes::from_static(chunk2), None);
        let result = body_writer
            .write_current_body_task(&mut mock_io)
            .await
            .unwrap();
        assert_eq!(result, Some(chunk2.len()));
        assert!(
            body_writer.finished(),
            "Should be finished after all bytes written"
        );

        // Finish should succeed since all content-length bytes were written
        body_writer.send_finish_task();
        let mut mock_io_finish = Builder::new().build();
        let result = body_writer
            .write_current_finish_task(&mut mock_io_finish)
            .await;
        assert!(
            result.is_ok(),
            "finish_task should succeed when all content-length bytes written"
        );
        assert!(matches!(body_writer.body_mode, BodyMode::Complete(_)));
    }
}
