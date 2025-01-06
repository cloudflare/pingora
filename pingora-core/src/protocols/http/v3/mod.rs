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

use std::fmt::Debug;
use http::{HeaderMap, HeaderName, HeaderValue, Request, Uri, Version};
use log::warn;
use quiche::h3::{Header, NameValue};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_error::{ErrorType, OrErr, Result};

pub mod server;
pub mod nohash;

pub fn event_to_request_headers(list: &Vec<Header>) -> Result<RequestHeader> {
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
    parts.uri = uri.build()
        .explain_err(ErrorType::H3Error, |_| "failed to convert event parts to request uri")?;
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

fn headermap_to_headervec(headers: &HeaderMap) -> Vec<Header> {
    headers
        .iter()
        .map(|(k, v)| Header::new(k.as_str().as_bytes(), v.as_bytes()))
        .collect()
}

fn header_size<T: NameValue + Debug>(headers: &[T]) -> usize {
    headers
        .iter()
        .fold(0, |acc, h| acc + h.value().len() + h.name().len() + 32)
}