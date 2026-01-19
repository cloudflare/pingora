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

//! Proxy Protocol support for preserving client connection information
//!
//! This module provides the [`ProxyProtocolReceiver`] trait and related types for implementing
//! [HAProxy's Proxy Protocol](https://www.haproxy.org/download/2.8/doc/proxy-protocol.txt).
//! The protocol allows intermediaries (like load balancers) to pass original client connection
//! information to backend servers.
//!
//! # Feature Flag
//!
//! This functionality requires the `proxy_protocol` feature to be enabled:
//! ```toml
//! [dependencies]
//! pingora-core = { version = "0.6", features = ["proxy_protocol"] }
//! ```
//!
//! # Protocol Versions
//!
//! Both Proxy Protocol v1 (human-readable text format) and v2 (binary format with TLV support)
//! are supported through the [`ProxyProtocolHeader`] enum.
//!
//! # Example
//!
//! ```rust,no_run
//! use async_trait::async_trait;
//! use pingora_core::protocols::proxy_protocol::{
//!     ProxyProtocolReceiver, ProxyProtocolHeader, HeaderV2,
//!     Command, Transport, Addresses
//! };
//! use pingora_core::protocols::l4::stream::Stream;
//! use pingora_error::Result;
//!
//! struct MyProxyProtocolParser;
//!
//! #[async_trait]
//! impl ProxyProtocolReceiver for MyProxyProtocolParser {
//!     async fn accept(&self, stream: &mut Stream) -> Result<(ProxyProtocolHeader, Vec<u8>)> {
//!         // Parse the Proxy Protocol header from the stream
//!         // Return the parsed header and any remaining bytes
//!         todo!("Implement parsing logic")
//!     }
//! }
//! ```

use async_trait::async_trait;
use std::borrow::Cow;
use std::net::SocketAddr;

use super::l4::stream::Stream;
use pingora_error::Result;

/// A trait for parsing Proxy Protocol headers from incoming connections.
///
/// Implementations of this trait handle reading and parsing Proxy Protocol headers
/// (v1 or v2) from a stream. The trait is designed to be flexible, allowing different
/// parsing strategies or third-party parser libraries to be used.
///
/// # Example
///
/// ```rust,no_run
/// use async_trait::async_trait;
/// use pingora_core::protocols::proxy_protocol::{
///     ProxyProtocolReceiver, ProxyProtocolHeader, HeaderV1,
///     Transport, Addresses
/// };
/// use pingora_core::protocols::l4::stream::Stream;
/// use pingora_error::Result;
/// use tokio::io::AsyncReadExt;
///
/// struct SimpleV1Parser;
///
/// #[async_trait]
/// impl ProxyProtocolReceiver for SimpleV1Parser {
///     async fn accept(&self, stream: &mut Stream) -> Result<(ProxyProtocolHeader, Vec<u8>)> {
///         let mut buffer = Vec::new();
///         // Read and parse v1 header
///         stream.read_buf(&mut buffer).await?;
///         // Parse logic here...
///         todo!("Parse v1 header and return result")
///     }
/// }
/// ```
///
/// # Performance Considerations
///
/// This method is called once per connection that uses Proxy Protocol. Implementations
/// should efficiently read only the necessary bytes from the stream to parse the header,
/// returning any excess bytes for subsequent processing.
#[async_trait]
pub trait ProxyProtocolReceiver: Send + Sync {
    /// Parses the Proxy Protocol header from an accepted connection stream.
    ///
    /// This method is called after a TCP connection is accepted on a Proxy Protocol endpoint.
    /// Implementors should read from the stream to parse the header according to either
    /// v1 (text) or v2 (binary) format specifications.
    ///
    /// # Arguments
    ///
    /// * `stream` - A mutable reference to the accepted connection stream
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// * The parsed [`ProxyProtocolHeader`] (v1 or v2)
    /// * Any remaining bytes read from the stream after the header (to be processed by the application)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The stream cannot be read
    /// * The header format is invalid
    /// * The connection is closed unexpectedly
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// async fn accept(&self, stream: &mut Stream) -> Result<(ProxyProtocolHeader, Vec<u8>)> {
    ///     // Read bytes from stream
    ///     let mut buffer = Vec::new();
    ///     stream.read_buf(&mut buffer).await?;
    ///
    ///     // Parse header and determine remaining bytes
    ///     let (header, remaining) = parse_proxy_header(&buffer)?;
    ///     Ok((header, remaining))
    /// }
    /// ```
    async fn accept(&self, stream: &mut Stream) -> Result<(ProxyProtocolHeader, Vec<u8>)>;
}

/// Parsed Proxy Protocol header containing connection information.
///
/// This enum represents either a v1 (text) or v2 (binary) Proxy Protocol header.
/// The version is determined by the parser implementation.
#[derive(Debug)]
pub enum ProxyProtocolHeader {
    /// Proxy Protocol version 1 (human-readable text format)
    V1(HeaderV1),
    /// Proxy Protocol version 2 (binary format with TLV extension support)
    V2(HeaderV2),
}

/// Proxy Protocol version 1 header information.
///
/// Version 1 uses a human-readable text format. It contains basic transport
/// and address information but does not support the command field or TLV extensions.
#[derive(Debug)]
pub struct HeaderV1 {
    /// The transport protocol used for the proxied connection
    pub transport: Transport,
    /// Source and destination addresses, if available.
    /// `None` indicates an unknown or local connection.
    pub addresses: Option<Addresses>,
}

/// Proxy Protocol version 2 header information.
///
/// Version 2 uses a binary format and supports additional features including
/// the command field (LOCAL vs PROXY) and optional TLV (Type-Length-Value) extensions
/// for passing custom metadata.
#[derive(Debug)]
pub struct HeaderV2 {
    /// Indicates whether this is a proxied connection or a local health check
    pub command: Command,
    /// The transport protocol used for the proxied connection
    pub transport: Transport,
    /// Source and destination addresses, if available.
    /// `None` for LOCAL command or unknown connections.
    pub addresses: Option<Addresses>,
    /// Optional TLV (Type-Length-Value) data for protocol extensions.
    /// May contain additional metadata such as SSL information, unique IDs, etc.
    pub tlvs: Option<Cow<'static, [u8]>>,
}

/// Transport protocol family for the proxied connection.
///
/// Indicates the network protocol (IPv4 or IPv6 over TCP) or unknown/unspecified transport.
#[derive(Debug)]
pub enum Transport {
    /// TCP over IPv4
    Tcp4,
    /// TCP over IPv6
    Tcp6,
    /// Unknown or unspecified transport protocol
    Unknown,
}

/// Source and destination socket addresses for a proxied connection.
///
/// Contains the original client address and the destination address
/// as seen by the proxy/load balancer.
#[derive(Debug)]
pub struct Addresses {
    /// The original source address (client)
    pub source: SocketAddr,
    /// The destination address as seen by the proxy
    pub destination: SocketAddr,
}

/// Proxy Protocol v2 command type.
///
/// Distinguishes between actual proxied connections and local connections
/// (typically used for health checks).
#[derive(Debug)]
pub enum Command {
    /// LOCAL command: indicates a health check or non-proxied connection.
    /// Receivers should not use any address information from LOCAL connections.
    Local,
    /// PROXY command: indicates a proxied connection with valid address information.
    Proxy,
}
