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

use std::{thread, time::Duration};

use futures_util::{SinkExt, StreamExt};
use log::debug;
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use pingora_http::RequestHeader;
use std::sync::LazyLock;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    runtime::Builder,
};

pub static WS_ECHO_RAW: LazyLock<bool> = LazyLock::new(init);
pub const WS_ECHO_RAW_ORIGIN_PORT: u16 = 9284;

fn init() -> bool {
    thread::spawn(move || {
        let runtime = Builder::new_current_thread()
            .thread_name("websocket raw echo")
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            server(&format!("127.0.0.1:{WS_ECHO_RAW_ORIGIN_PORT}"))
                .await
                .unwrap();
        })
    });
    thread::sleep(Duration::from_millis(200));
    true
}

async fn server(addr: &str) -> Result<(), Error> {
    let listener = TcpListener::bind(&addr).await.unwrap();
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(handle_connection(stream));
    }
    Ok(())
}

async fn read_request_header(stream: &mut TcpStream) -> Result<(RequestHeader, Vec<u8>)> {
    fn parse_request_header(buf: &[u8]) -> Result<RequestHeader> {
        let mut headers = vec![httparse::EMPTY_HEADER; 256];
        let mut parsed = httparse::Request::new(&mut headers);
        match parsed
            .parse(buf)
            .or_err(ReadError, "request header parse error")?
        {
            httparse::Status::Complete(_) => {
                let mut req = RequestHeader::build(
                    parsed.method.unwrap_or(""),
                    parsed.path.unwrap_or("").as_bytes(),
                    Some(parsed.headers.len()),
                )?;
                for header in parsed.headers.iter() {
                    req.append_header(header.name.to_string(), header.value)
                        .unwrap();
                }
                Ok(req)
            }
            _ => Error::e_explain(ReadError, "should have full request header"),
        }
    }

    let mut request = vec![];
    let mut header_end = 0;
    let mut buf = [0; 1024];
    loop {
        let n = stream
            .read(&mut buf)
            .await
            .or_err(ReadError, "while reading request header")?;
        request.extend_from_slice(&buf[..n]);
        let mut end_of_header = false;
        for (i, w) in request.windows(4).enumerate() {
            if w == b"\r\n\r\n" {
                end_of_header = true;
                header_end = i + 4;
                break;
            }
        }
        if end_of_header {
            break;
        }
    }
    Ok((
        parse_request_header(&request[..header_end])?,
        request[header_end..].to_vec(),
    ))
}

async fn read_body_until_close(
    stream: &mut OwnedReadHalf,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut buf = [0; 1024];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(buf[..n].to_vec()))
}

async fn write_body_until_close(
    stream: &mut OwnedWriteHalf,
    body: &[u8],
) -> Result<Option<usize>, std::io::Error> {
    let n = stream.write(body).await?;
    Ok((n != 0).then_some(n))
}

async fn handle_connection(mut stream: TcpStream) -> Result<()> {
    let (header, preread_body) = read_request_header(&mut stream).await?;

    // if x-expected-body-len unset, continue to read until stream is closed
    let expected_body_len = header
        .headers
        .get("x-expected-body-len")
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .and_then(|s| s.parse().ok());

    let resp_raw =
        b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n";
    stream
        .write_all(resp_raw)
        .await
        .or_err(WriteError, "while writing 101")?;

    let (mut stream_read, mut stream_write) = stream.into_split();
    let mut request_body = preread_body;
    let mut body_read = request_body.len();
    let mut body_read_done = false;

    loop {
        tokio::select! {
            res = read_body_until_close(&mut stream_read), if !body_read_done => {
                let Some(buf) = res.or_err(ReadError, "while reading body")? else {
                    return Ok(());
                };
                body_read += buf.len();
                body_read_done = expected_body_len.is_some_and(|len| body_read >= len);
                request_body.extend_from_slice(&buf[..]);
            }
            res = write_body_until_close(&mut stream_write, &request_body[..]), if !request_body.is_empty() => {
                let Some(n) = res.or_err(WriteError, "while writing body")? else {
                    return Ok(());
                };
                request_body = request_body[n..].to_vec();
            }
            else => break,
        }
    }
    if let Some(expected) = expected_body_len {
        if body_read > expected {
            return Error::e_explain(ReadError, "read {body_read} bytes, expected {expected}");
        }
    }
    Ok(())
}
