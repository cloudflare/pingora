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

//! Subrequest body reader and writer.
//!
//! This implementation is very similar to v1 if not identical in many cases.
//! However it is generally much simpler because it does not have to handle
//! wire format bytes, simply basic checks such as content-length and when the
//! underlying channel (sender or receiver) is closed.

use bytes::Bytes;
use log::{debug, trace, warn};
use pingora_error::{
    Error,
    ErrorType::{self, *},
    OrErr, Result,
};
use std::fmt::Debug;
use tokio::sync::{mpsc, oneshot};

use crate::protocols::http::HttpTask;
use http::HeaderMap;

pub const PREMATURE_BODY_END: ErrorType = ErrorType::new("PrematureBodyEnd");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseState {
    ToStart,
    Complete(usize),       // total size
    Partial(usize, usize), // size read, remaining size
    Done(usize),           // done but there is error, size read
    UntilClose(usize),     // read until connection closed, size read
}

type PS = ParseState;

pub struct BodyReader {
    pub body_state: ParseState,
    notify_wants_body: Option<oneshot::Sender<()>>,
}

impl BodyReader {
    pub fn new(notify_wants_body: Option<oneshot::Sender<()>>) -> Self {
        BodyReader {
            body_state: PS::ToStart,
            notify_wants_body,
        }
        // TODO: if wants body signal is None, init empty
    }

    pub fn need_init(&self) -> bool {
        matches!(self.body_state, PS::ToStart)
    }

    pub fn init_content_length(&mut self, cl: usize) {
        match cl {
            0 => self.body_state = PS::Complete(0),
            _ => {
                self.body_state = PS::Partial(0, cl);
            }
        }
    }

    pub fn init_until_close(&mut self) {
        self.body_state = PS::UntilClose(0);
    }

    pub fn body_done(&self) -> bool {
        matches!(self.body_state, PS::Complete(_) | PS::Done(_))
    }

    pub fn body_empty(&self) -> bool {
        self.body_state == PS::Complete(0)
    }

    pub async fn read_body(&mut self, rx: &mut mpsc::Receiver<HttpTask>) -> Result<Option<Bytes>> {
        match self.body_state {
            PS::Complete(_) => Ok(None),
            PS::Done(_) => Ok(None),
            PS::Partial(_, _) => self.do_read_body(rx).await,
            PS::UntilClose(_) => self.do_read_body_until_closed(rx).await,
            PS::ToStart => panic!("need to init BodyReader first"),
        }
    }

    pub async fn do_read_body(
        &mut self,
        rx: &mut mpsc::Receiver<HttpTask>,
    ) -> Result<Option<Bytes>> {
        if let Some(notify) = self.notify_wants_body.take() {
            // fine if downstream isn't actively being read
            let _ = notify.send(());
        }
        let (bytes, end) = match rx.recv().await {
            Some(HttpTask::Body(bytes, end)) => (bytes, end),
            Some(task) => {
                // TODO: return an error into_down for Failed?
                return Error::e_explain(
                    InternalError,
                    format!("Unexpected HttpTask {task:?} while reading body (subrequest)"),
                );
            }
            None => (None, true), // downstream ended
        };

        match self.body_state {
            PS::Partial(read, to_read) => {
                let n = bytes.as_ref().map_or(0, |b| b.len());
                debug!(
                    "BodyReader body_state: {:?}, read data from IO: {n} (subrequest)",
                    self.body_state,
                );
                if bytes.is_none() {
                    self.body_state = PS::Done(read);
                    return Error::e_explain(ConnectionClosed, format!(
                        "Peer prematurely closed connection with {to_read} bytes of body remaining to read (subrequest)",
                    ));
                }
                if end && n < to_read {
                    // TODO: this doesn't flush the bytes we did receive to upstream
                    self.body_state = PS::Done(read + n);
                    return Error::e_explain(PREMATURE_BODY_END, format!(
                        "Peer prematurely ended body with {} bytes of body remaining to read (subrequest)",
                        to_read - n
                    ));
                }
                if n >= to_read {
                    if n > to_read {
                        warn!(
                            "Peer sent more data then expected: extra {}\
                               bytes, discarding them (subrequest)",
                            n - to_read
                        );
                    }
                    self.body_state = PS::Complete(read + to_read);
                    Ok(bytes.map(|b| b.slice(0..to_read)))
                } else {
                    self.body_state = PS::Partial(read + n, to_read - n);
                    Ok(bytes)
                }
            }
            _ => panic!("wrong body state: {:?} (subrequest)", self.body_state),
        }
    }

    pub async fn do_read_body_until_closed(
        &mut self,
        rx: &mut mpsc::Receiver<HttpTask>,
    ) -> Result<Option<Bytes>> {
        if let Some(notify) = self.notify_wants_body.take() {
            // fine if downstream isn't active, receiver will indicate this
            let _ = notify.send(());
        }

        let (bytes, end) = match rx.recv().await {
            Some(HttpTask::Body(bytes, end)) => (bytes, end),
            Some(task) => {
                return Error::e_explain(
                    InternalError,
                    format!("Unexpected HttpTask {task:?} while reading body (subrequest)"),
                );
            }
            None => (None, true), // downstream ended
        };
        let n = bytes.as_ref().map_or(0, |b| b.len());
        match self.body_state {
            PS::UntilClose(read) => {
                if bytes.is_none() {
                    self.body_state = PS::Complete(read);
                    Ok(None)
                } else if end {
                    // explicit end also signifies completion
                    self.body_state = PS::Complete(read + n);
                    Ok(bytes)
                } else {
                    self.body_state = PS::UntilClose(read + n);
                    Ok(bytes)
                }
            }
            _ => panic!("wrong body state: {:?} (subrequest)", self.body_state),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyMode {
    ToSelect,
    ContentLength(usize, usize), // total length to write, bytes already written
    UntilClose(usize),           //bytes written
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

    pub fn init_until_close(&mut self) {
        self.body_mode = BM::UntilClose(0);
    }

    pub fn init_content_length(&mut self, cl: usize) {
        self.body_mode = BM::ContentLength(cl, 0);
    }

    pub async fn write_body(
        &mut self,
        sender: &mut mpsc::Sender<HttpTask>,
        bytes: Bytes,
    ) -> Result<Option<usize>> {
        trace!("Writing Body, size: {} (subrequest)", bytes.len());
        match self.body_mode {
            BM::Complete(_) => Ok(None),
            BM::ContentLength(_, _) => self.do_write_body(sender, bytes).await,
            BM::UntilClose(_) => self.do_write_until_close_body(sender, bytes).await,
            BM::ToSelect => panic!("wrong body phase: ToSelect (subrequest)"),
        }
    }

    pub fn finished(&self) -> bool {
        match self.body_mode {
            BM::Complete(_) => true,
            BM::ContentLength(total, written) => written >= total,
            _ => false,
        }
    }

    async fn do_write_body(
        &mut self,
        tx: &mut mpsc::Sender<HttpTask>,
        bytes: Bytes,
    ) -> Result<Option<usize>> {
        match self.body_mode {
            BM::ContentLength(total, written) => {
                if written >= total {
                    // already written full length
                    return Ok(None);
                }
                let mut to_write = total - written;
                if to_write < bytes.len() {
                    warn!("Trying to write data over content-length (subrequest): {total}");
                } else {
                    to_write = bytes.len();
                }
                let res = tx.send(HttpTask::Body(Some(bytes), false)).await;
                match res {
                    Ok(()) => {
                        self.body_mode = BM::ContentLength(total, written + to_write);
                        Ok(Some(to_write))
                    }
                    Err(e) => Error::e_because(WriteError, "while writing body (subrequest)", e),
                }
            }
            _ => panic!("wrong body mode: {:?} (subrequest)", self.body_mode),
        }
    }

    async fn do_write_until_close_body(
        &mut self,
        tx: &mut mpsc::Sender<HttpTask>,
        bytes: Bytes,
    ) -> Result<Option<usize>> {
        match self.body_mode {
            BM::UntilClose(written) => {
                let res = tx.send(HttpTask::Body(Some(bytes.clone()), false)).await;
                match res {
                    Ok(()) => {
                        self.body_mode = BM::UntilClose(written + bytes.len());
                        Ok(Some(bytes.len()))
                    }
                    Err(e) => Error::e_because(WriteError, "while writing body (subrequest)", e),
                }
            }
            _ => panic!("wrong body mode: {:?} (subrequest)", self.body_mode),
        }
    }

    pub async fn finish(&mut self, sender: &mut mpsc::Sender<HttpTask>) -> Result<Option<usize>> {
        match self.body_mode {
            BM::Complete(_) => Ok(None),
            BM::ContentLength(_, _) => self.do_finish_body(sender).await,
            BM::UntilClose(_) => self.do_finish_until_close_body(sender).await,
            BM::ToSelect => Ok(None),
        }
    }

    async fn do_finish_body(&mut self, tx: &mut mpsc::Sender<HttpTask>) -> Result<Option<usize>> {
        match self.body_mode {
            BM::ContentLength(total, written) => {
                self.body_mode = BM::Complete(written);
                if written < total {
                    return Error::e_explain(
                        PREMATURE_BODY_END,
                        format!("Content-length: {total} bytes written: {written} (subrequest)"),
                    );
                }
                tx.send(HttpTask::Done).await.or_err(
                    WriteError,
                    "while sending done task to downstream (subrequest)",
                )?;
                Ok(Some(written))
            }
            _ => panic!("wrong body mode: {:?} (subrequest)", self.body_mode),
        }
    }

    async fn do_finish_until_close_body(
        &mut self,
        tx: &mut mpsc::Sender<HttpTask>,
    ) -> Result<Option<usize>> {
        match self.body_mode {
            BM::UntilClose(written) => {
                self.body_mode = BM::Complete(written);
                tx.send(HttpTask::Done).await.or_err(
                    WriteError,
                    "while sending done task to downstream (subrequest)",
                )?;
                Ok(Some(written))
            }
            _ => panic!("wrong body mode: {:?} (subrequest)", self.body_mode),
        }
    }

    pub async fn write_trailers(
        &mut self,
        tx: &mut mpsc::Sender<HttpTask>,
        trailers: Option<Box<HeaderMap>>,
    ) -> Result<()> {
        // TODO more safeguards e.g. trailers after end of stream
        tx.send(HttpTask::Trailer(trailers)).await.or_err(
            WriteError,
            "while writing response trailers to downstream (subrequest)",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    const TASK_BUFFER_SIZE: usize = 4;

    #[tokio::test]
    async fn read_with_body_content_length() {
        init_log();
        let input = b"abc";
        let (tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        let mut body_reader = BodyReader::new(None);
        body_reader.init_content_length(3);

        tx.send(HttpTask::Body(Some(Bytes::from(&input[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input[..]);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
    }

    #[tokio::test]
    async fn read_with_body_content_length_2() {
        init_log();
        let input1 = b"a";
        let input2 = b"bc";
        let (tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        let mut body_reader = BodyReader::new(None);
        body_reader.init_content_length(3);

        tx.send(HttpTask::Body(Some(Bytes::from(&input1[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input1[..]);
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));

        tx.send(HttpTask::Body(Some(Bytes::from(&input2[..])), true))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input2[..]);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
    }

    #[tokio::test]
    async fn read_with_body_content_length_empty_task() {
        init_log();
        let input1 = b"a";
        let input2 = b""; // zero length body task
        let (tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        let mut body_reader = BodyReader::new(None);
        body_reader.init_content_length(3);

        tx.send(HttpTask::Body(Some(Bytes::from(&input1[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input1[..]);
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));

        // subrequest can allow empty body tasks
        tx.send(HttpTask::Body(Some(Bytes::from(&input2[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input2[..]);
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));

        // premature end of stream still errors
        tx.send(HttpTask::Body(Some(Bytes::from(&input2[..])), true))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap_err();
        assert_eq!(&PREMATURE_BODY_END, res.etype());
        assert_eq!(body_reader.body_state, ParseState::Done(1));
    }

    #[tokio::test]
    async fn read_with_body_content_length_less() {
        init_log();
        let input1 = b"a";
        let (tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        let mut body_reader = BodyReader::new(None);
        body_reader.init_content_length(3);

        tx.send(HttpTask::Body(Some(Bytes::from(&input1[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input1[..]);
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));

        drop(tx);
        let res = body_reader.read_body(&mut rx).await.unwrap_err();
        assert_eq!(&ConnectionClosed, res.etype());
        assert_eq!(body_reader.body_state, ParseState::Done(1));
    }

    #[tokio::test]
    async fn read_with_body_content_length_more() {
        init_log();
        let input1 = b"a";
        let input2 = b"bcd";
        let (tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);

        let mut body_reader = BodyReader::new(None);
        body_reader.init_content_length(3);

        tx.send(HttpTask::Body(Some(Bytes::from(&input1[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input1[..]);
        assert_eq!(body_reader.body_state, ParseState::Partial(1, 2));

        tx.send(HttpTask::Body(Some(Bytes::from(&input2[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input2[0..2]);
        assert_eq!(body_reader.body_state, ParseState::Complete(3));
    }

    #[tokio::test]
    async fn read_with_body_until_close() {
        init_log();
        let input1 = b"a";
        let input2 = b""; // zero length body but not actually close
        let (tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);
        let mut body_reader = BodyReader::new(None);
        body_reader.init_until_close();

        tx.send(HttpTask::Body(Some(Bytes::from(&input1[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input1[..]);
        assert_eq!(body_reader.body_state, ParseState::UntilClose(1));

        tx.send(HttpTask::Body(Some(Bytes::from(&input2[..])), false))
            .await
            .unwrap();
        let res = body_reader.read_body(&mut rx).await.unwrap().unwrap();
        assert_eq!(res, &input2[..]);
        assert_eq!(body_reader.body_state, ParseState::UntilClose(1));

        // sending end closed
        drop(tx);
        let res = body_reader.read_body(&mut rx).await.unwrap();
        assert_eq!(res, None);
        assert_eq!(body_reader.body_state, ParseState::Complete(1));
    }

    #[tokio::test]
    async fn write_body_cl() {
        init_log();
        let output = b"a";
        let (mut tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);
        let mut body_writer = BodyWriter::new();
        body_writer.init_content_length(1);
        assert_eq!(body_writer.body_mode, BodyMode::ContentLength(1, 0));
        let res = body_writer
            .write_body(&mut tx, Bytes::from(&output[..]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::ContentLength(1, 1));
        // write again, over the limit
        let res = body_writer
            .write_body(&mut tx, Bytes::from(&output[..]))
            .await
            .unwrap();
        assert_eq!(res, None);
        assert_eq!(body_writer.body_mode, BodyMode::ContentLength(1, 1));
        let res = body_writer.finish(&mut tx).await.unwrap().unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::Complete(1));

        // only one body task written
        match rx.try_recv().unwrap() {
            HttpTask::Body(b, end) => {
                assert_eq!(b.unwrap(), &output[..]);
                assert!(!end);
            }
            task => panic!("unexpected task {task:?}"),
        }
        assert!(matches!(rx.try_recv().unwrap(), HttpTask::Done));
        drop(tx);

        assert_eq!(
            rx.try_recv().unwrap_err(),
            mpsc::error::TryRecvError::Disconnected
        );
    }

    #[tokio::test]
    async fn write_body_until_close() {
        init_log();
        let data = b"a";
        let (mut tx, mut rx) = mpsc::channel::<HttpTask>(TASK_BUFFER_SIZE);
        let mut body_writer = BodyWriter::new();
        body_writer.init_until_close();
        assert_eq!(body_writer.body_mode, BodyMode::UntilClose(0));
        let res = body_writer
            .write_body(&mut tx, Bytes::from(&data[..]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::UntilClose(1));
        match rx.try_recv().unwrap() {
            HttpTask::Body(b, end) => {
                assert_eq!(b.unwrap().as_ref(), data);
                assert!(!end);
            }
            task => panic!("unexpected task {task:?}"),
        }

        let res = body_writer
            .write_body(&mut tx, Bytes::from(&data[..]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, 1);
        assert_eq!(body_writer.body_mode, BodyMode::UntilClose(2));
        let res = body_writer.finish(&mut tx).await.unwrap().unwrap();
        assert_eq!(res, 2);
        assert_eq!(body_writer.body_mode, BodyMode::Complete(2));
        match rx.try_recv().unwrap() {
            HttpTask::Body(b, end) => {
                assert_eq!(b.unwrap().as_ref(), data);
                assert!(!end);
            }
            task => panic!("unexpected task {task:?}"),
        }
        assert!(matches!(rx.try_recv().unwrap(), HttpTask::Done));

        assert_eq!(rx.try_recv().unwrap_err(), mpsc::error::TryRecvError::Empty);
    }
}
