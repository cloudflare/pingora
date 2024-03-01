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

use crate::*;
use bytes::Bytes;
use http::header;

#[derive(Debug, Clone)]
pub struct CaseHeaderName(Bytes);

impl CaseHeaderName {
    pub fn new(name: String) -> Self {
        CaseHeaderName(name.into())
    }
}

impl CaseHeaderName {
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn from_slice(buf: &[u8]) -> Self {
        CaseHeaderName(Bytes::copy_from_slice(buf))
    }
}

/// A trait that converts into case-sensitive header names.
pub trait IntoCaseHeaderName {
    fn into_case_header_name(self) -> CaseHeaderName;
}

impl IntoCaseHeaderName for CaseHeaderName {
    fn into_case_header_name(self) -> CaseHeaderName {
        self
    }
}

impl IntoCaseHeaderName for String {
    fn into_case_header_name(self) -> CaseHeaderName {
        CaseHeaderName(self.into())
    }
}

impl IntoCaseHeaderName for &'static str {
    fn into_case_header_name(self) -> CaseHeaderName {
        CaseHeaderName(self.into())
    }
}

impl IntoCaseHeaderName for HeaderName {
    fn into_case_header_name(self) -> CaseHeaderName {
        CaseHeaderName(titled_header_name(&self))
    }
}

impl IntoCaseHeaderName for &HeaderName {
    fn into_case_header_name(self) -> CaseHeaderName {
        CaseHeaderName(titled_header_name(self))
    }
}

impl IntoCaseHeaderName for Bytes {
    fn into_case_header_name(self) -> CaseHeaderName {
        CaseHeaderName(self)
    }
}

fn titled_header_name(header_name: &HeaderName) -> Bytes {
    titled_header_name_str(header_name).map_or_else(
        || Bytes::copy_from_slice(header_name.as_str().as_bytes()),
        |s| Bytes::from_static(s.as_bytes()),
    )
}

pub(crate) fn titled_header_name_str(header_name: &HeaderName) -> Option<&'static str> {
    Some(match *header_name {
        header::AGE => "Age",
        header::CACHE_CONTROL => "Cache-Control",
        header::CONNECTION => "Connection",
        header::CONTENT_TYPE => "Content-Type",
        header::CONTENT_ENCODING => "Content-Encoding",
        header::CONTENT_LENGTH => "Content-Length",
        header::DATE => "Date",
        header::TRANSFER_ENCODING => "Transfer-Encoding",
        header::HOST => "Host",
        header::SERVER => "Server",
        // TODO: add more const header here to map to their titled case
        // TODO: automatically upper case the first letter?
        _ => {
            return None;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_case_header_name() {
        assert_eq!("FoO".into_case_header_name().as_slice(), b"FoO");
        assert_eq!("FoO".to_string().into_case_header_name().as_slice(), b"FoO");
        assert_eq!(header::SERVER.into_case_header_name().as_slice(), b"Server");
    }
}
