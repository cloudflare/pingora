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

//! Training to generate the zstd dictionary.

use std::fs;
use zstd::dict;

/// Train the zstd dictionary from all the files under the given `dir_path`
///
/// The output will be the trained dictionary
pub fn train<P: AsRef<std::path::Path>>(dir_path: P) -> Vec<u8> {
    // TODO: check f is file, it can be dir
    let files = fs::read_dir(dir_path)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|f| f.path()));
    dict::from_files(files, 64 * 1024 * 1024).unwrap()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::resp_header_to_buf;
    use pingora_http::ResponseHeader;

    fn gen_test_dict() -> Vec<u8> {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("samples/test");
        train(path)
    }

    fn gen_test_header() -> ResponseHeader {
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .append_header("Date", "Thu, 23 Dec 2021 11:23:29 GMT")
            .unwrap();
        header
            .append_header("Last-Modified", "Sat, 09 Oct 2021 22:41:34 GMT")
            .unwrap();
        header.append_header("Connection", "keep-alive").unwrap();
        header.append_header("Vary", "Accept-encoding").unwrap();
        header.append_header("Content-Encoding", "gzip").unwrap();
        header
            .append_header("Access-Control-Allow-Origin", "*")
            .unwrap();
        header
    }

    #[test]
    fn test_ser_with_dict() {
        let dict = gen_test_dict();
        let serde = crate::HeaderSerde::new(Some(dict));
        let serde_no_dict = crate::HeaderSerde::new(None);
        let header = gen_test_header();

        let compressed = serde.serialize(&header).unwrap();
        let compressed_no_dict = serde_no_dict.serialize(&header).unwrap();
        let mut buf = vec![];
        let uncompressed = resp_header_to_buf(&header, &mut buf);

        assert!(compressed.len() < uncompressed);
        assert!(compressed.len() < compressed_no_dict.len());
    }

    #[test]
    fn test_ser_de_with_dict() {
        let dict = gen_test_dict();
        let serde = crate::HeaderSerde::new(Some(dict));
        let header = gen_test_header();

        let compressed = serde.serialize(&header).unwrap();
        let header2 = serde.deserialize(&compressed).unwrap();

        assert_eq!(header.status, header2.status);
        assert_eq!(header.headers, header2.headers);
    }
}
