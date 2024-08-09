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

use bytes::{BufMut, Bytes, BytesMut};
use http::{
    header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING},
    HeaderMap,
};
use pingora_error::{ErrorType::ReadError, OrErr, Result};
use pingora_http::{RequestHeader, ResponseHeader};

/// Used for bridging gRPC to gRPC-web and vice-versa.
/// See gRPC-web [spec](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-WEB.md) and
/// gRPC h2 [spec](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md) for more details.
#[derive(Default, PartialEq, Debug)]
pub enum GrpcWebCtx {
    #[default]
    Disabled,
    Init,
    Upgrade,
    Trailers,
    Done,
}

const GRPC: &str = "application/grpc";
const GRPC_WEB: &str = "application/grpc-web";

impl GrpcWebCtx {
    pub fn init(&mut self) {
        *self = Self::Init;
    }

    /// gRPC-web request is fed into this filter, if the module is initialized
    /// we attempt to convert it to a gRPC request
    pub fn request_header_filter(&mut self, req: &mut RequestHeader) {
        if *self != Self::Init {
            // not enabled
            return;
        }

        let content_type = req
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        // check we have a valid grpc-web prefix
        if !(content_type.len() >= GRPC_WEB.len()
            && content_type[..GRPC_WEB.len()].eq_ignore_ascii_case(GRPC_WEB))
        {
            // not gRPC-web
            return;
        }

        // change content type to grpc
        let ct = content_type.to_lowercase().replace(GRPC_WEB, GRPC);
        req.insert_header(CONTENT_TYPE, ct).expect("insert header");

        // The 'te' request header is used to detect incompatible proxies
        // which are supposed to remove 'te' if it is unsupported.
        // This header is required by gRPC over h2 protocol.
        // https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md
        req.insert_header("te", "trailers").expect("insert header");

        // For gRPC requests, EOS (end-of-stream) is indicated by the presence of the
        // END_STREAM flag on the last received DATA frame.
        // In scenarios where the Request stream needs to be closed
        // but no data remains to be sent implementations
        // MUST send an empty DATA frame with this flag set.
        req.set_send_end_stream(false);

        *self = Self::Upgrade
    }

    /// gRPC response is fed into this filter, if the module is in the bridge state
    /// attempt to convert the response it to a gRPC-web response
    pub fn response_header_filter(&mut self, resp: &mut ResponseHeader) {
        if *self != Self::Upgrade {
            // not an upgrade
            return;
        }

        if resp.status.is_informational() {
            // proxy informational statuses through
            return;
        }

        let content_type = resp
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();

        // upstream h2, no reason to normalize case
        if !content_type.starts_with(GRPC) {
            // not gRPC
            *self = Self::Disabled;
            return;
        }

        // change content type to gRPC-web
        let ct = content_type.replace(GRPC, GRPC_WEB);
        resp.insert_header(CONTENT_TYPE, ct).expect("insert header");

        // always use chunked for gRPC-web
        resp.remove_header(&CONTENT_LENGTH);
        resp.insert_header(TRANSFER_ENCODING, "chunked")
            .expect("insert header");

        *self = Self::Trailers
    }

    /// Used to convert gRPC trailers into gRPC-web trailers, note
    /// gRPC-web trailers are encoded into the response body so we return
    /// the encoded bytes here.
    pub fn response_trailer_filter(
        &mut self,
        resp_trailers: &mut HeaderMap,
    ) -> Result<Option<Bytes>> {
        /* Trailer header frame and trailer headers
            0 - - 1 - - 2 - - 3 - - 4 - - 5 - - 6 - - 7 - - 8
            | Ind |        Length         |     Headers     | <- trailer header indicator, length of headers
            |                    Headers                    | <- rest is headers
            |                    Headers                    |
        */
        // TODO compressed trailer?
        // grpc-web trailers frame head
        const GRPC_WEB_TRAILER: u8 = 0x80;

        // number of bytes in trailer header
        const GRPC_TRAILER_HEADER_LEN: usize = 5;

        // just some estimate
        const DEFAULT_TRAILER_BUFFER_SIZE: usize = 256;

        if *self != Self::Trailers {
            // not an upgrade
            *self = Self::Disabled;
            return Ok(None);
        }

        // trailers are expected to arrive all at once encoded into a single trailers frame
        // trailers in frame are separated by CRLFs
        let mut buf = BytesMut::with_capacity(DEFAULT_TRAILER_BUFFER_SIZE);
        let mut trailers = buf.split_off(GRPC_TRAILER_HEADER_LEN);

        // iterate the key/value pairs and encode them into the tmp buffer
        for (key, value) in resp_trailers.iter() {
            // encode header
            trailers.put_slice(key.as_ref());
            trailers.put_slice(b":");

            // encode value
            trailers.put_slice(value.as_ref());

            // encode header separator
            trailers.put_slice(b"\r\n");
        }

        // ensure trailer length within u32
        let len = trailers.len().try_into().or_err_with(ReadError, || {
            format!("invalid gRPC trailer length: {}", trailers.len())
        })?;
        buf.put_u8(GRPC_WEB_TRAILER);
        buf.put_u32(len);
        buf.unsplit(trailers);

        *self = Self::Done;
        Ok(Some(buf.freeze()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{request::Request, response::Response, Version};

    #[test]
    fn non_grpc_web_request_ignored() {
        let request = Request::get("https://pingora.dev/")
            .header(CONTENT_TYPE, "application/grpc-we")
            .version(Version::HTTP_2) // only set this to verify send_end_stream is configured
            .body(())
            .unwrap();
        let mut request = request.into_parts().0.into();

        let mut filter = GrpcWebCtx::default();
        filter.init();
        filter.request_header_filter(&mut request);
        assert_eq!(filter, GrpcWebCtx::Init);

        let headers = &request.headers;
        assert_eq!(headers.get("te"), None);
        assert_eq!(headers.get("application/grpc"), None);
        assert_eq!(request.send_end_stream(), Some(true));
    }

    #[test]
    fn grpc_web_request_module_disabled_ignored() {
        let request = Request::get("https://pingora.dev/")
            .header(CONTENT_TYPE, "application/grpc-web")
            .version(Version::HTTP_2) // only set this to verify send_end_stream is configured
            .body(())
            .unwrap();
        let mut request = request.into_parts().0.into();

        // do not init
        let mut filter = GrpcWebCtx::default();
        filter.request_header_filter(&mut request);
        assert_eq!(filter, GrpcWebCtx::Disabled);

        let headers = &request.headers;
        assert_eq!(headers.get("te"), None);
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/grpc-web");
        assert_eq!(request.send_end_stream(), Some(true));
    }

    #[test]
    fn grpc_web_request_upgrade() {
        let request = Request::get("https://pingora.org/")
            .header(CONTENT_TYPE, "application/gRPC-web+thrift")
            .version(Version::HTTP_2) // only set this to verify send_end_stream is configured
            .body(())
            .unwrap();
        let mut request = request.into_parts().0.into();

        let mut filter = GrpcWebCtx::default();
        filter.init();
        filter.request_header_filter(&mut request);
        assert_eq!(filter, GrpcWebCtx::Upgrade);

        let headers = &request.headers;
        assert_eq!(headers.get("te").unwrap(), "trailers");
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap(),
            "application/grpc+thrift"
        );
        assert_eq!(request.send_end_stream(), Some(false));
    }

    #[test]
    fn non_grpc_response_ignored() {
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/html")
            .header(CONTENT_LENGTH, "10")
            .body(())
            .unwrap();
        let mut response = response.into_parts().0.into();

        let mut filter = GrpcWebCtx::Upgrade;
        filter.response_header_filter(&mut response);
        assert_eq!(filter, GrpcWebCtx::Disabled);

        let headers = &response.headers;
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "text/html");
        assert_eq!(headers.get(CONTENT_LENGTH).unwrap(), "10");
    }

    #[test]
    fn grpc_response_module_disabled_ignored() {
        let response = Response::builder()
            .header(CONTENT_TYPE, "application/grpc")
            .body(())
            .unwrap();
        let mut response = response.into_parts().0.into();

        let mut filter = GrpcWebCtx::default();
        filter.response_header_filter(&mut response);
        assert_eq!(filter, GrpcWebCtx::Disabled);

        let headers = &response.headers;
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/grpc");
    }

    #[test]
    fn grpc_response_upgrade() {
        let response = Response::builder()
            .header(CONTENT_TYPE, "application/grpc+proto")
            .header(CONTENT_LENGTH, "0")
            .body(())
            .unwrap();
        let mut response = response.into_parts().0.into();

        let mut filter = GrpcWebCtx::Upgrade;
        filter.response_header_filter(&mut response);
        assert_eq!(filter, GrpcWebCtx::Trailers);

        let headers = &response.headers;
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap(),
            "application/grpc-web+proto"
        );
        assert_eq!(headers.get(TRANSFER_ENCODING).unwrap(), "chunked");
        assert!(headers.get(CONTENT_LENGTH).is_none());
    }

    #[test]
    fn grpc_response_informational_proxied() {
        let response = Response::builder().status(100).body(()).unwrap();
        let mut response = response.into_parts().0.into();

        let mut filter = GrpcWebCtx::Upgrade;
        filter.response_header_filter(&mut response);
        assert_eq!(filter, GrpcWebCtx::Upgrade); // still upgrade
    }

    #[test]
    fn grpc_response_trailer_headers_convert_to_byte_buf() {
        let mut response = Response::builder()
            .header("grpc-status", "0")
            .header("grpc-message", "OK")
            .body(())
            .unwrap();
        let response = response.headers_mut();

        let mut filter = GrpcWebCtx::Trailers;
        let buf = filter.response_trailer_filter(response).unwrap().unwrap();
        assert_eq!(filter, GrpcWebCtx::Done);

        let expected = b"grpc-status:0\r\ngrpc-message:OK\r\n";
        let expected_len: u32 = expected.len() as u32; // 32 bytes

        // assert the length prefix message frame
        // [1 byte (header)| 4 byte (length) | 15 byte (grpc-status:0\r\n) | 17 bytes (grpc-message:OK\r\n)]
        assert_eq!(0x80, buf[0]); // frame should start with trailer header
        assert_eq!(expected_len.to_be_bytes(), buf[1..5]); // next 4 bytes length of trailer
        assert_eq!(expected[..15], buf[5..20]); // grpc-status:0\r\n (15 bytes)
        assert_eq!(expected[15..], buf[20..]); // grpc-message:OK\r\n (17 bytes)
    }
}
