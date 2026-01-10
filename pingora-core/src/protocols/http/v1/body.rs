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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocols::l4::stream::AsyncWriteVec;
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
    // HTTP1_0: read until connection closed, size read
    HTTP1_0(usize),
}

type PS = ParseState;

impl ParseState {
    pub fn finish(&self, additional_bytes: usize) -> Self {
        match self {
            PS::Partial(read, to_read) => PS::Complete(read + to_read),
            PS::Chunked(read, _, _, _) => PS::Complete(read + additional_bytes),
            PS::ChunkedFinal(read, _, _, _) => PS::Complete(read + additional_bytes),
            PS::HTTP1_0(read) => PS::Complete(read + additional_bytes),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn done(&self, additional_bytes: usize) -> Self {
        match self {
            PS::Partial(read, _) => PS::Done(read + additional_bytes),
            PS::Chunked(read, _, _, _) => PS::Done(read + additional_bytes),
            PS::ChunkedFinal(read, _, _, _) => PS::Done(read + additional_bytes),
            PS::HTTP1_0(read) => PS::Done(read + additional_bytes),
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
            0 => self.body_state = PS::Complete(0),
            _ => {
                self.prepare_buf(buf_to_rewind);
                self.body_state = PS::Partial(0, cl);
            }
        }
    }

    pub fn init_http10(&mut self, buf_to_rewind: &[u8]) {
        self.prepare_buf(buf_to_rewind);
        self.body_state = PS::HTTP1_0(0);
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
            PS::HTTP1_0(_) => self.do_read_body_until_closed(stream).await,
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
            PS::HTTP1_0(read) => {
                if n == 0 {
                    self.body_state = PS::Complete(read);
                    Ok(None)
                } else {
                    self.body_state = PS::HTTP1_0(read + n);
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
    HTTP1_0(usize),              //bytes written
    Complete(usize),             //bytes written
}

type BM = BodyMode;

pub struct BodyWriter {
    pub body_mode: BodyMode,
}

impl BodyWriter {
    pub fn new() -> Self {
        BodyWriter {
            body_mode: BM::ToSelect,
        }
    }

    pub fn init_chunked(&mut self) {
        self.body_mode = BM::ChunkedEncoding(0);
    }

    pub fn init_http10(&mut self) {
        self.body_mode = BM::HTTP1_0(0);
    }

    pub fn init_content_length(&mut self, cl: usize) {
        self.body_mode = BM::ContentLength(cl, 0);
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
            BM::HTTP1_0(_) => self.do_write_http1_0_body(stream, buf).await,
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

    async fn do_write_http1_0_body<S>(
        &mut self,
        stream: &mut S,
        buf: &[u8],
    ) -> Result<Option<usize>>
    where
        S: AsyncWrite + Unpin + Send,
    {
        match self.body_mode {
            BM::HTTP1_0(written) => {
                let res = stream.write_all(buf).await;
                match res {
                    Ok(()) => {
                        self.body_mode = BM::HTTP1_0(written + buf.len());
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
            BM::HTTP1_0(_) => self.do_finish_http1_0_body(stream),
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

    fn do_finish_http1_0_body<S>(&mut self, _stream: &mut S) -> Result<Option<usize>> {
        match self.body_mode {
            BM::HTTP1_0(written) => {
                self.body_mode = BM::Complete(written);
                Ok(Some(written))
            }
            _ => panic!("wrong body mode: {:?}", self.body_mode),
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
        body_reader.init_http10(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::HTTP1_0(1));
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
        body_reader.init_http10(rewind);
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::HTTP1_0(2));
        assert_eq!(rewind, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::HTTP1_0(3));
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
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1)); // input1 concat input2
        assert_eq!(&input2[1..2], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 6, 11, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
        assert_eq!(body_reader.get_body_overread(), None);
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
    async fn write_body_http10() {
        init_log();
        let data = b"a";
        let mut mock_io = Builder::new().write(&data[..]).write(&data[..]).build();
        let mut body_writer = BodyWriter::new();
        body_writer.init_http10();
        assert_eq!(body_writer.body_mode, BodyMode::HTTP1_0(0));
        let res = body_writer
            .write_body(&mut mock_io, &data[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::HTTP1_0(1));
        let res = body_writer
            .write_body(&mut mock_io, &data[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::HTTP1_0(2));
        let res = body_writer.finish(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, 2);
        assert_eq!(body_writer.body_mode, BodyMode::Complete(2));
    }
}
