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

//! HTTP/3 implementation

use http::{HeaderMap, HeaderName, HeaderValue, Request, Uri, Version};
use log::{trace, warn};
use pingora_error::{Error, ErrorType, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use quiche::h3::{Event, Header, NameValue};
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use http::uri::{Authority, Scheme};
use parking_lot::Mutex;
use quiche::Connection;
use tokio::sync::mpsc::Receiver;
use tokio::sync::Notify;
use pingora_error::ErrorType::{H3Error, InvalidHTTPHeader, ReadError};

pub const H3_SESSION_EVENTS_CHANNEL_SIZE: usize = 256;
pub const H3_SESSION_DROP_DEQUE_INITIAL_CAPACITY: usize = 2048;

pub mod client;
pub mod nohash;
pub mod server;

fn event_to_request_headers(list: &Vec<Header>) -> Result<RequestHeader> {
    let (mut parts, _) = Request::new(()).into_parts();
    let mut uri = Uri::builder();
    let mut headers = HeaderMap::new();

    for h in list {
        match h.name() {
            b":scheme" => uri = uri.scheme(h.value()),
            b":authority" => uri = uri.authority(h.value()),
            b":path" => uri = uri.path_and_query(h.value()),
            b":method" => match h.value().try_into() {
                Ok(v) => parts.method = v,
                Err(_) => warn!("Failed to parse method from input: {:?}", h.value()),
            },
            _ => match HeaderName::from_bytes(h.name()) {
                Ok(k) => match HeaderValue::from_bytes(h.value()) {
                    Ok(v) => {
                        headers.append(k, v);
                    }
                    Err(_) => warn!("Failed to parse header value from input: {:?}", h.value()),
                },
                Err(_) => warn!("Failed to parse header name input: {:?}", h.name()),
            },
        }
    }

    parts.version = Version::HTTP_3;
    parts.uri = uri.build().explain_err(ErrorType::H3Error, |_| {
        "failed to convert event parts to request uri"
    })?;
    parts.headers = headers;
    Ok(parts.into())
}

fn response_headers_to_event(resp: &ResponseHeader) -> Vec<Header> {
    let mut qheaders: Vec<Header> = Vec::with_capacity(resp.headers.len() + 1);
    qheaders.push(Header::new(b":status", resp.status.as_str().as_bytes()));

    for (k, v) in &resp.headers {
        qheaders.push(Header::new(k.as_str().as_bytes(), v.as_bytes()))
    }
    qheaders
}

fn request_headers_to_event(req: &RequestHeader) -> Result<Vec<Header>> {
    let mut qheaders: Vec<Header> = Vec::with_capacity(req.headers.len() + 4);
    // only encrypted traffic supported in HTTP3
    qheaders.push(Header::new(b":scheme".as_slice(),  Scheme::HTTPS.to_string().as_bytes()));

    // use authority when present
    let authority = if let Some(authority) = req.uri.authority() {
        authority.clone()
    } else {
        // or use host header as authority
        let host = req.headers.get(http::header::HOST);
        let Some(host) = host else {
            return Error::e_explain(InvalidHTTPHeader, "no authority header for h3");
        };
        // validate
        Authority::try_from(host.as_bytes())
            .explain_err(InvalidHTTPHeader, |_| format!("invalid authority from host {:?}", host))?
    };
    qheaders.push(Header::new(b":authority".as_slice(), authority.as_str().as_bytes()));

    let Some(path) = req.uri.path_and_query() else {
        return Error::e_explain(InvalidHTTPHeader, "no path header for h3");
    };
    qheaders.push(Header::new(b":path".as_slice(), path.as_str().as_bytes()));
    qheaders.push(Header::new(b":method".as_slice(), req.method.as_str().as_bytes()));

    // copy all other request headers
    // the pseudo-headers starting with ":" need to be sent before regular headers
    for (k, v) in &req.headers {
        qheaders.push(Header::new(k.as_str().as_bytes(), v.as_bytes()))
    }
    Ok(qheaders)
}

fn event_to_response_headers(resp: &Vec<Header>) -> Result<ResponseHeader> {
    // pseudo-headers have to be first, response only has a single valid pseudo header ":status"
    // which MUST be included as per RFC9114 Section 4.3.2
    let mut response = ResponseHeader::build(resp[0].value(), Some(resp.len() - 1))?;
    response.set_version(Version::HTTP_3);

    for h in &resp[1..] {
        let k = HeaderName::from_bytes(h.name())
            .explain_err(InvalidHTTPHeader,
                         |_| format!("failed to parse header name {:?}", h.name()))?;
        let v = HeaderValue::from_bytes(h.value())
            .explain_err(InvalidHTTPHeader,
                         |_| format!("failed to parse header value {:?}", h.value()))?;
        response.append_header(k, v)?;
    }

    Ok(response)
}

fn headermap_to_headervec(headers: &HeaderMap) -> Vec<Header> {
    headers
        .iter()
        .map(|(k, v)| Header::new(k.as_str().as_bytes(), v.as_bytes()))
        .collect()
}

fn headervec_to_headermap(headers: &Vec<Header>) -> Result<HeaderMap> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for h in headers {
        if h.name().len() > 0 && h.name()[0] == b":".as_slice()[0] {
            let k = HeaderName::from_bytes(h.name())
                .explain_err(InvalidHTTPHeader,
                             |_| format!("failed to parse header name {:?}", h.name()))?;
            let v = HeaderValue::from_bytes(h.value())
                .explain_err(InvalidHTTPHeader,
                             |_| format!("failed to parse header value {:?}", h.value()))?;
            map.insert(k, v);
        }
    }
    Ok(map)
}

fn header_size<T: NameValue + Debug>(headers: &[T]) -> usize {
    headers
        .iter()
        .fold(0, |acc, h| acc + h.value().len() + h.name().len() + 32)
}

fn stream_capacity<'a>(
    conn: &'a Mutex<Connection>,
    stream_id: u64,
    required: usize,
    rx_notify: &'a Notify,
    tx_notify: &'a Notify
) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>> {
    Box::pin(async move {
        let capacity;
        {
            let qconn = conn.lock();
            let conn_id = qconn.trace_id();
            capacity = qconn.stream_capacity(stream_id)
                .explain_err(ErrorType::WriteError, |e| {
                    format!(
                        "H3 connection {} failed to acquire capacity for stream {} error {:?}",
                        conn_id, stream_id, e
                    )
                })?;
        }

        // FIXME: handle capacity <= required e.g. required is gt configured send buffers
        if capacity >= required {
            Ok(capacity)
        } else {
            tx_notify.notify_waiters();
            rx_notify.notified().await;
            stream_capacity(conn, stream_id, required, rx_notify, tx_notify).await
        }
    })
}

async fn data_finished_event(stream_id: u64, event_rx: &mut Receiver<Event>) -> Result<()> {
    loop {
        match event_rx.recv().await {
            Some(ev) => {
                match ev {
                    Event::Finished => {
                        trace!("stream {} event {:?}", stream_id, ev);
                        return Ok(());
                    }
                    Event::Headers { .. } => {
                        debug_assert!(false, "Headers or Finished event when Data requested");
                    }
                    Event::Data => {
                        trace!("stream {} event {:?}", stream_id, ev);
                        return Ok(());
                    }
                    Event::Reset(error_code) => {
                        return Err(Error::explain(
                            H3Error,
                            format!("stream was reset with error code {}", error_code),
                        ))
                    }
                    Event::PriorityUpdate => {
                        // TODO: this step should be deferred until
                        // h3::Connection::poll() returns Error::Done
                        // see also h3::Connection::send_response_with_priority()

                        /*
                        // https://datatracker.ietf.org/doc/rfc9218/
                        let mut hconn = self.h3_connection.lock();
                        // field value has the same content as the header::Priority field
                        let field_value = hconn.take_last_priority_update(self.stream_id)
                            .explain_err(H3Error, "failed to receive priority update field value")?;
                        */
                        warn!("received unhandled priority update");
                        continue;
                    }
                    Event::GoAway => {
                        // RFC 9114 Section 5.2 & 7.2.6
                        warn!("received unhandled go-away");
                        continue;
                    }
                }
            }
            None => {
                return Err(Error::explain(
                    ReadError,
                    "H3 session event channel disconnected",
                ))
            }
        }
    }
}