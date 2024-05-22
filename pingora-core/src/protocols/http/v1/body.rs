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

const LAST_CHUNK: &[u8; 5] = b"0\r\n\r\n";

pub const INVALID_CHUNK: ErrorType = ErrorType::new("InvalidChunk");
pub const PREMATURE_BODY_END: ErrorType = ErrorType::new("PrematureBodyEnd");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseState {
    ToStart,
    Complete(usize),                     // total size
    Partial(usize, usize),               // size read, remaining size
    Chunked(usize, usize, usize, usize), // size read, next to read in current buf start, read in current buf start, remaining chucked size to read from IO
    Done(usize),                         // done but there is error, size read
    HTTP1_0(usize),                      // read until connection closed, size read
}

type PS = ParseState;

impl ParseState {
    pub fn finish(&self, additional_bytes: usize) -> Self {
        match self {
            PS::Partial(read, to_read) => PS::Complete(read + to_read),
            PS::Chunked(read, _, _, _) => PS::Complete(read + additional_bytes),
            PS::HTTP1_0(read) => PS::Complete(read + additional_bytes),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn done(&self, additional_bytes: usize) -> Self {
        match self {
            PS::Partial(read, _) => PS::Done(read + additional_bytes),
            PS::Chunked(read, _, _, _) => PS::Done(read + additional_bytes),
            PS::HTTP1_0(read) => PS::Done(read + additional_bytes),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn partial_chunk(&self, bytes_read: usize, bytes_to_read: usize) -> Self {
        match self {
            PS::Chunked(read, _, _, _) => PS::Chunked(read + bytes_read, 0, 0, bytes_to_read),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn multi_chunk(&self, bytes_read: usize, buf_start_index: usize) -> Self {
        match self {
            PS::Chunked(read, _, buf_end, _) => {
                PS::Chunked(read + bytes_read, buf_start_index, *buf_end, 0)
            }
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn partial_chunk_head(&self, head_end: usize, head_size: usize) -> Self {
        match self {
            /* inform reader to read more to form a legal chunk */
            PS::Chunked(read, _, _, _) => PS::Chunked(*read, 0, head_end, head_size),
            _ => self.clone(), /* invalid transaction */
        }
    }

    pub fn new_buf(&self, buf_end: usize) -> Self {
        match self {
            PS::Chunked(read, _, _, _) => PS::Chunked(*read, 0, buf_end, 0),
            _ => self.clone(), /* invalid transaction */
        }
    }
}

pub struct BodyReader {
    pub body_state: ParseState,
    pub body_buf: Option<BytesMut>,
    pub body_buf_size: usize,
    rewind_buf_len: usize,
}

impl BodyReader {
    pub fn new() -> Self {
        BodyReader {
            body_state: PS::ToStart,
            body_buf: None,
            body_buf_size: BODY_BUFFER_SIZE,
            rewind_buf_len: 0,
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

    pub fn body_done(&self) -> bool {
        matches!(self.body_state, PS::Complete(_) | PS::Done(_))
    }

    pub fn body_empty(&self) -> bool {
        self.body_state == PS::Complete(0)
    }

    pub async fn read_body<S>(&mut self, stream: &mut S) -> Result<Option<BufRef>>
    where
        S: AsyncRead + Unpin + Send,
    {
        match self.body_state {
            PS::Complete(_) => Ok(None),
            PS::Done(_) => Ok(None),
            PS::Partial(_, _) => self.do_read_body(stream).await,
            PS::Chunked(_, _, _, _) => self.do_read_chunked_body(stream).await,
            PS::HTTP1_0(_) => self.do_read_body_until_closed(stream).await,
            PS::ToStart => panic!("need to init BodyReader first"),
        }
    }

    pub async fn do_read_body<S>(&mut self, stream: &mut S) -> Result<Option<BufRef>>
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
                        trace!(
                            "partial chunk payload, expecting_from_io: {}, \
                                existing_buf_end {}, buf: {:?}",
                            expecting_from_io,
                            existing_buf_end,
                            String::from_utf8_lossy(
                                &self.body_buf.as_ref().unwrap()[..existing_buf_end]
                            )
                        );
                        // partial chunk payload, will read more
                        if expecting_from_io >= existing_buf_end + 2 {
                            // not enough
                            self.body_state = self.body_state.partial_chunk(
                                existing_buf_end,
                                expecting_from_io - existing_buf_end,
                            );
                            return Ok(Some(BufRef::new(0, existing_buf_end)));
                        }
                        /* could be expecting DATA + CRLF or just CRLF */
                        let payload_size = if expecting_from_io > 2 {
                            expecting_from_io - 2
                        } else {
                            0
                        };
                        /* expecting_from_io < existing_buf_end + 2 */
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
                    self.parse_chunked_buf(existing_buf_start, existing_buf_end)
                }
            }
            _ => panic!("wrong body state: {:?}", self.body_state),
        }
    }

    fn parse_chunked_buf(
        &mut self,
        buf_index_start: usize,
        buf_index_end: usize,
    ) -> Result<Option<BufRef>> {
        let buf = &self.body_buf.as_ref().unwrap()[buf_index_start..buf_index_end];
        let chunk_status = httparse::parse_chunk_size(buf);
        match chunk_status {
            Ok(status) => {
                match status {
                    httparse::Status::Complete((payload_index, chunk_size)) => {
                        // TODO: Check chunk_size overflow
                        trace!(
                            "Got size {chunk_size}, payload_index: {payload_index}, chunk: {:?}",
                            String::from_utf8_lossy(buf)
                        );
                        let chunk_size = chunk_size as usize;
                        if chunk_size == 0 {
                            /* terminating chunk. TODO: trailer */
                            self.body_state = self.body_state.finish(0);
                            return Ok(None);
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
                            self.body_state = self
                                .body_state
                                .partial_chunk(actual_size, chunk_end_index - buf.len());
                            return Ok(Some(BufRef::new(
                                buf_index_start + payload_index,
                                actual_size,
                            )));
                        }
                        /* got multiple chunks, return the first */
                        self.body_state = self
                            .body_state
                            .multi_chunk(chunk_size, buf_index_start + chunk_end_index);
                        Ok(Some(BufRef::new(
                            buf_index_start + payload_index,
                            chunk_size,
                        )))
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
                            Ok(Some(BufRef::new(0, 0)))
                        }
                    }
                }
            }
            Err(e) => {
                let context = format!("Invalid chucked encoding: {e:?}");
                debug!("{context}, {:?}", String::from_utf8_lossy(buf));
                self.body_state = self.body_state.done(0);
                Error::e_explain(INVALID_CHUNK, context)
            }
        }
    }
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
        let mut body_reader = BodyReader::new();
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 3));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(input, body_reader.get_body(&res));
    }

    #[tokio::test]
    async fn read_with_body_content_length_2() {
        init_log();
        let input1 = b"a";
        let input2 = b"bc";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(input2, body_reader.get_body(&res));
    }

    #[tokio::test]
    async fn read_with_body_content_length_less() {
        init_log();
        let input1 = b"a";
        let input2 = b""; // simulating close
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap_err();
        assert_eq!(&ConnectionClosed, res.etype());
        assert_eq!(body_reader.body_state, ParseState::Done(1));
    }

    #[tokio::test]
    async fn read_with_body_content_length_more() {
        init_log();
        let input1 = b"a";
        let input2 = b"bcd";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_content_length(3, b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(&input2[0..2], body_reader.get_body(&res));
    }

    #[tokio::test]
    async fn read_with_body_content_length_rewind() {
        init_log();
        let rewind = b"ab";
        let input = b"c";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_content_length(3, rewind);
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 2));
        assert_eq!(body_reader.body_state, ParseState::Partial(2, 1));
        assert_eq!(rewind, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
        assert_eq!(input, body_reader.get_body(&res));
    }

    #[tokio::test]
    async fn read_with_body_http10() {
        init_log();
        let input1 = b"a";
        let input2 = b""; // simulating close
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_http10(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(0, 1));
        assert_eq!(body_reader.body_state, ParseState::HTTP1_0(1));
        assert_eq!(input1, body_reader.get_body(&res));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
    }

    #[tokio::test]
    async fn read_with_body_http10_rewind() {
        init_log();
        let rewind = b"ab";
        let input1 = b"c";
        let input2 = b""; // simulating close
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
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
    }

    #[tokio::test]
    async fn read_with_body_zero_chunk() {
        init_log();
        let input = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
    }

    #[tokio::test]
    async fn read_with_body_chunk_ext() {
        init_log();
        let input = b"0;aaaa\r\n\r\n";
        let mut mock_io = Builder::new().read(&input[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(0));
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
        let mut body_reader = BodyReader::new();
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
    }

    #[tokio::test]
    async fn read_with_body_1_chunk() {
        init_log();
        let input1 = b"1\r\na\r\n";
        let input2 = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
        body_reader.init_chunked(b"");
        let res = body_reader.read_body(&mut mock_io).await.unwrap().unwrap();
        assert_eq!(res, BufRef::new(3, 1));
        assert_eq!(&input1[3..4], body_reader.get_body(&res));
        assert_eq!(body_reader.body_state, ParseState::Chunked(1, 0, 0, 0));
        let res = body_reader.read_body(&mut mock_io).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
    }

    #[tokio::test]
    async fn read_with_body_1_chunk_rewind() {
        init_log();
        let rewind = b"1\r\nx\r\n";
        let input1 = b"1\r\na\r\n";
        let input2 = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
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
    }

    #[tokio::test]
    async fn read_with_body_multi_chunk() {
        init_log();
        let input1 = b"1\r\na\r\n2\r\nbc\r\n";
        let input2 = b"0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
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
    }

    #[tokio::test]
    async fn read_with_body_partial_chunk() {
        init_log();
        let input1 = b"3\r\na";
        let input2 = b"bc\r\n0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
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
    }

    #[tokio::test]
    async fn read_with_body_partial_head_chunk() {
        init_log();
        let input1 = b"1\r";
        let input2 = b"\na\r\n0\r\n\r\n";
        let mut mock_io = Builder::new().read(&input1[..]).read(&input2[..]).build();
        let mut body_reader = BodyReader::new();
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
