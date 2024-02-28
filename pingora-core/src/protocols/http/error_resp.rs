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

//! Error response generating utilities.

use http::header;
use once_cell::sync::Lazy;
use pingora_http::ResponseHeader;

use super::SERVER_NAME;

/// Generate an error response with the given status code.
///
/// This error response has a zero `Content-Length` and `Cache-Control: private, no-store`.
pub fn gen_error_response(code: u16) -> ResponseHeader {
    let mut resp = ResponseHeader::build(code, Some(4)).unwrap();
    resp.insert_header(header::SERVER, &SERVER_NAME[..])
        .unwrap();
    resp.insert_header(header::DATE, "Sun, 06 Nov 1994 08:49:37 GMT")
        .unwrap(); // placeholder
    resp.insert_header(header::CONTENT_LENGTH, "0").unwrap();
    resp.insert_header(header::CACHE_CONTROL, "private, no-store")
        .unwrap();
    resp
}

/// Pre-generated 502 response
pub static HTTP_502_RESPONSE: Lazy<ResponseHeader> = Lazy::new(|| gen_error_response(502));
/// Pre-generated 400 response
pub static HTTP_400_RESPONSE: Lazy<ResponseHeader> = Lazy::new(|| gen_error_response(400));
