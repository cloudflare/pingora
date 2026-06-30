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

//! Training to generate the zstd dictionary.

use pingora_error::ErrorType::InternalError;
use pingora_error::{OrErr, Result};
use std::fs;
use zstd::dict;

/// Train the zstd dictionary from all the **files** (not directories) under
/// the given `dir_path`.
///
/// Returns the trained dictionary bytes, or an error if the directory cannot
/// be read or the training itself fails.
pub fn train<P: AsRef<std::path::Path>>(dir_path: P) -> Result<Vec<u8>> {
    // Collect only regular files; skip subdirectories and unreadable entries.
    let files: Vec<_> = fs::read_dir(dir_path)
        .explain_err(InternalError, |_| "failed to read training directory")?
        .filter_map(|entry| {
            entry.ok().and_then(|f| {
                let path = f.path();
                path.is_file().then_some(path)
            })
        })
        .collect();

    dict::from_files(files, 64 * 1024 * 1024)
        .explain_err(InternalError, |_| "failed to train zstd dictionary")
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::resp_header_to_buf;
    use pingora_http::ResponseHeader;

    fn gen_test_dict() -> Vec<u8> {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("samples/test");
        train(path).expect("test dict training should succeed")
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
    fn test_train_skips_subdirectories() {
        // The samples/test directory contains only files; confirm train()
        // returns Ok without panicking even when invoked on a known-good dir.
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("samples/test");
        assert!(train(path).is_ok());
    }
}
