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
use log::warn;
use quiche::h3::{Header, NameValue};
use pingora_http::{RequestHeader, ResponseHeader};

pub mod server;
pub mod nohash;

pub fn event_to_request_headers(list: &Vec<Header>) -> RequestHeader {
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
                Err(_) => {
                    warn!("Failed to parse method from input: {:?}", h.value())
                }
            },
            _ => {
                match HeaderName::from_bytes(h.name()) {
                    Ok(k) => match HeaderValue::from_bytes(h.value()) {
                        Ok(v) => {
                            headers.append(k, v);
                        }
                        Err(_) => {
                            warn!("Failed to parse header value from input: {:?}", h.value())
                        }
                    },
                    Err(_) => {
                        warn!("Failed to parse header name input: {:?}", h.name())
                    }
                };
            }
        }
    }

    parts.version = Version::HTTP_3;
    parts.uri = uri.build().unwrap(); // TODO: use result
    parts.headers = headers;
    parts.into()
}

#[allow(unused)] // TODO: remove
fn response_headers_to_event(resp: &ResponseHeader) -> Vec<Header> {
    let mut qheaders: Vec<Header> = Vec::with_capacity(resp.headers.len() + 1);
    qheaders.push(Header::new(b":status", resp.status.as_str().as_bytes()));

    for (k, v) in &resp.headers {
        qheaders.push(Header::new(k.as_str().as_bytes(), v.as_bytes()))
    }
    qheaders
}