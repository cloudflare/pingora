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

//! Integration tests for ClientHello extraction

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use pingora_core::protocols::l4::stream::Stream;
    use pingora_core::protocols::ClientHelloWrapper;
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;

    // Minimal TLS ClientHello with SNI for "example.com"
    const CLIENT_HELLO_WITH_SNI: &[u8] = &[
        0x16, // Content Type: Handshake
        0x03, 0x01, // Version: TLS 1.0
        0x00, 0x5d, // Length
        0x01, // Handshake Type: ClientHello
        0x00, 0x00, 0x59, // Length
        0x03, 0x03, // Version: TLS 1.2
        // Random (32 bytes)
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f, 0x00, // Session ID Length
        0x00, 0x04, // Cipher Suites Length
        0x00, 0x2f, 0x00, 0x35, // Cipher suites
        0x01, // Compression Methods Length
        0x00, // Compression method
        0x00, 0x18, // Extensions Length
        // SNI Extension
        0x00, 0x00, // Extension Type: server_name
        0x00, 0x14, // Extension Length
        0x00, 0x12, // Server Name List Length
        0x00, // Server Name Type: host_name
        0x00, 0x0f, // Server Name Length
        // "example.com" (11 bytes) - padded to 15
        0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x63, 0x6f, 0x6d, 0x00, 0x00, 0x00, 0x00,
    ];

    #[tokio::test]
    async fn test_extract_client_hello_from_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        // Server task
        let server_task = tokio::spawn(async move {
            let (tcp_stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;

            let stream: Stream = tcp_stream.into();
            let mut wrapper = ClientHelloWrapper::new(stream);

            // Extract ClientHello
            let hello = wrapper.extract_client_hello().unwrap();
            assert!(hello.is_some());

            let hello = hello.unwrap();
            assert_eq!(hello.sni, Some("example.com".to_string()));
            assert_eq!(hello.tls_version, Some(0x0301));
        });

        // Client task
        let client_task = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            notify.notify_one();

            // Wait a bit to ensure server is ready
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            // Send ClientHello
            stream.write_all(CLIENT_HELLO_WITH_SNI).await.unwrap();
            stream.flush().await.unwrap();

            // Keep connection open for a bit
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        });

        // Wait for both tasks
        let _ = tokio::join!(server_task, client_task);
    }

    #[tokio::test]
    async fn test_wrapper_with_non_tls_data() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        // Server task
        let server_task = tokio::spawn(async move {
            let (tcp_stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;

            let stream: Stream = tcp_stream.into();
            let mut wrapper = ClientHelloWrapper::new(stream);

            // Try to extract ClientHello from non-TLS data
            let hello = wrapper.extract_client_hello().unwrap();
            // Should return None for non-TLS data
            assert!(hello.is_none());
        });

        // Client task
        let client_task = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            notify.notify_one();

            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            // Send non-TLS data
            stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
            stream.flush().await.unwrap();

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        });

        let _ = tokio::join!(server_task, client_task);
    }

    #[tokio::test]
    async fn test_wrapper_passthrough() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        let test_data = b"Hello, World!";

        // Server task
        let server_task = tokio::spawn(async move {
            let (tcp_stream, _) = listener.accept().await.unwrap();
            let stream: Stream = tcp_stream.into();
            let mut wrapper = ClientHelloWrapper::new(stream);

            notify2.notified().await;

            // Read through wrapper
            let mut buf = vec![0u8; test_data.len()];
            wrapper.read_exact(&mut buf).await.unwrap();

            assert_eq!(&buf, test_data);
        });

        // Client task
        let client_task = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            notify.notify_one();

            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            stream.write_all(test_data).await.unwrap();
            stream.flush().await.unwrap();
        });

        let _ = tokio::join!(server_task, client_task);
    }

    #[tokio::test]
    async fn test_extract_then_read() {
        use tokio::io::AsyncReadExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        // Server task
        let server_task = tokio::spawn(async move {
            let (tcp_stream, _) = listener.accept().await.unwrap();
            notify2.notified().await;

            let stream: Stream = tcp_stream.into();
            let mut wrapper = ClientHelloWrapper::new(stream);

            // Extract ClientHello (using MSG_PEEK)
            let hello = wrapper.extract_client_hello().unwrap();
            assert!(hello.is_some());
            let hello = hello.unwrap();
            assert_eq!(hello.sni, Some("example.com".to_string()));

            // Now read the same data (it should still be in the buffer)
            let mut buf = vec![0u8; CLIENT_HELLO_WITH_SNI.len()];
            wrapper.read_exact(&mut buf).await.unwrap();

            // The data read should match what was sent
            assert_eq!(&buf, CLIENT_HELLO_WITH_SNI);
        });

        // Client task
        let client_task = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            notify.notify_one();

            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

            stream.write_all(CLIENT_HELLO_WITH_SNI).await.unwrap();
            stream.flush().await.unwrap();

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        });

        let _ = tokio::join!(server_task, client_task);
    }
}

