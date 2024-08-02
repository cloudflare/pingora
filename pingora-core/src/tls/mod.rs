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

//! This module contains a dummy TLS implementation for the scenarios where real TLS
//! implementations are unavailable.

macro_rules! impl_display {
    ($ty:ty) => {
        impl std::fmt::Display for $ty {
            fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
                Ok(())
            }
        }
    };
}

macro_rules! impl_deref {
    ($from:ty => $to:ty) => {
        impl std::ops::Deref for $from {
            type Target = $to;
            fn deref(&self) -> &$to {
                panic!("Not implemented");
            }
        }
        impl std::ops::DerefMut for $from {
            fn deref_mut(&mut self) -> &mut $to {
                panic!("Not implemented");
            }
        }
    };
}

pub mod ssl {
    use super::error::ErrorStack;
    use super::x509::verify::X509VerifyParamRef;
    use super::x509::{X509VerifyResult, X509};

    /// An error returned from an ALPN selection callback.
    pub struct AlpnError;
    impl AlpnError {
        /// Terminate the handshake with a fatal alert.
        pub const ALERT_FATAL: AlpnError = Self {};

        /// Do not select a protocol, but continue the handshake.
        pub const NOACK: AlpnError = Self {};
    }

    /// A type which allows for configuration of a client-side TLS session before connection.
    pub struct ConnectConfiguration;
    impl_deref! {ConnectConfiguration => SslRef}
    impl ConnectConfiguration {
        /// Configures the use of Server Name Indication (SNI) when connecting.
        pub fn set_use_server_name_indication(&mut self, _use_sni: bool) {
            panic!("Not implemented");
        }

        /// Configures the use of hostname verification when connecting.
        pub fn set_verify_hostname(&mut self, _verify_hostname: bool) {
            panic!("Not implemented");
        }

        /// Returns an `Ssl` configured to connect to the provided domain.
        pub fn into_ssl(self, _domain: &str) -> Result<Ssl, ErrorStack> {
            panic!("Not implemented");
        }

        /// Like `SslContextBuilder::set_verify`.
        pub fn set_verify(&mut self, _mode: SslVerifyMode) {
            panic!("Not implemented");
        }

        /// Like `SslContextBuilder::set_alpn_protos`.
        pub fn set_alpn_protos(&mut self, _protocols: &[u8]) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Returns a mutable reference to the X509 verification configuration.
        pub fn param_mut(&mut self) -> &mut X509VerifyParamRef {
            panic!("Not implemented");
        }
    }

    /// An SSL error.
    #[derive(Debug)]
    pub struct Error;
    impl_display!(Error);
    impl Error {
        pub fn code(&self) -> ErrorCode {
            panic!("Not implemented");
        }
    }

    /// An error code returned from SSL functions.
    #[derive(PartialEq)]
    pub struct ErrorCode(i32);
    impl ErrorCode {
        /// An error occurred in the SSL library.
        pub const SSL: ErrorCode = Self(0);
    }

    /// An identifier of a session name type.
    pub struct NameType;
    impl NameType {
        pub const HOST_NAME: NameType = Self {};
    }

    /// The state of an SSL/TLS session.
    pub struct Ssl;
    impl Ssl {
        /// Creates a new `Ssl`.
        pub fn new(_ctx: &SslContextRef) -> Result<Ssl, ErrorStack> {
            panic!("Not implemented");
        }
    }
    impl_deref! {Ssl => SslRef}

    /// A type which wraps server-side streams in a TLS session.
    pub struct SslAcceptor;
    impl SslAcceptor {
        /// Creates a new builder configured to connect to non-legacy clients. This should
        /// generally be considered a reasonable default choice.
        pub fn mozilla_intermediate_v5(
            _method: SslMethod,
        ) -> Result<SslAcceptorBuilder, ErrorStack> {
            panic!("Not implemented");
        }
    }

    /// A builder for `SslAcceptor`s.
    pub struct SslAcceptorBuilder;
    impl SslAcceptorBuilder {
        /// Consumes the builder, returning a `SslAcceptor`.
        pub fn build(self) -> SslAcceptor {
            panic!("Not implemented");
        }

        /// Sets the callback used by a server to select a protocol for Application Layer Protocol
        /// Negotiation (ALPN).
        pub fn set_alpn_select_callback<F>(&mut self, _callback: F)
        where
            F: for<'a> Fn(&mut SslRef, &'a [u8]) -> Result<&'a [u8], AlpnError>
                + 'static
                + Sync
                + Send,
        {
            panic!("Not implemented");
        }

        /// Loads a certificate chain from a file.
        pub fn set_certificate_chain_file<P: AsRef<std::path::Path>>(
            &mut self,
            _file: P,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Loads the private key from a file.
        pub fn set_private_key_file<P: AsRef<std::path::Path>>(
            &mut self,
            _file: P,
            _file_type: SslFiletype,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Sets the maximum supported protocol version.
        pub fn set_max_proto_version(
            &mut self,
            _version: Option<SslVersion>,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }
    }

    /// Reference to an [`SslCipher`].
    pub struct SslCipherRef;
    impl SslCipherRef {
        /// Returns the name of the cipher.
        pub fn name(&self) -> &'static str {
            panic!("Not implemented");
        }
    }

    /// A type which wraps client-side streams in a TLS session.
    pub struct SslConnector;
    impl SslConnector {
        /// Creates a new builder for TLS connections.
        pub fn builder(_method: SslMethod) -> Result<SslConnectorBuilder, ErrorStack> {
            panic!("Not implemented");
        }

        /// Returns a structure allowing for configuration of a single TLS session before connection.
        pub fn configure(&self) -> Result<ConnectConfiguration, ErrorStack> {
            panic!("Not implemented");
        }

        /// Returns a shared reference to the inner raw `SslContext`.
        pub fn context(&self) -> &SslContextRef {
            panic!("Not implemented");
        }
    }

    /// A builder for `SslConnector`s.
    pub struct SslConnectorBuilder;
    impl SslConnectorBuilder {
        /// Consumes the builder, returning an `SslConnector`.
        pub fn build(self) -> SslConnector {
            panic!("Not implemented");
        }

        /// Sets the list of supported ciphers for protocols before TLSv1.3.
        pub fn set_cipher_list(&mut self, _cipher_list: &str) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Sets the context’s supported signature algorithms.
        pub fn set_sigalgs_list(&mut self, _sigalgs: &str) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Sets the minimum supported protocol version.
        pub fn set_min_proto_version(
            &mut self,
            _version: Option<SslVersion>,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Sets the maximum supported protocol version.
        pub fn set_max_proto_version(
            &mut self,
            _version: Option<SslVersion>,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Use the default locations of trusted certificates for verification.
        pub fn set_default_verify_paths(&mut self) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Loads trusted root certificates from a file.
        pub fn set_ca_file<P: AsRef<std::path::Path>>(
            &mut self,
            _file: P,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Loads a leaf certificate from a file.
        pub fn set_certificate_file<P: AsRef<std::path::Path>>(
            &mut self,
            _file: P,
            _file_type: SslFiletype,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Loads the private key from a file.
        pub fn set_private_key_file<P: AsRef<std::path::Path>>(
            &mut self,
            _file: P,
            _file_type: SslFiletype,
        ) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Sets the TLS key logging callback.
        pub fn set_keylog_callback<F>(&mut self, _callback: F)
        where
            F: Fn(&SslRef, &str) + 'static + Sync + Send,
        {
            panic!("Not implemented");
        }
    }

    /// A context object for TLS streams.
    pub struct SslContext;
    impl SslContext {
        /// Creates a new builder object for an `SslContext`.
        pub fn builder(_method: SslMethod) -> Result<SslContextBuilder, ErrorStack> {
            panic!("Not implemented");
        }
    }
    impl_deref! {SslContext => SslContextRef}

    /// A builder for `SslContext`s.
    pub struct SslContextBuilder;
    impl SslContextBuilder {
        /// Consumes the builder, returning a new `SslContext`.
        pub fn build(self) -> SslContext {
            panic!("Not implemented");
        }
    }

    /// Reference to [`SslContext`]
    pub struct SslContextRef;

    /// An identifier of the format of a certificate or key file.
    pub struct SslFiletype;
    impl SslFiletype {
        /// The PEM format.
        pub const PEM: SslFiletype = Self {};
    }

    /// A type specifying the kind of protocol an `SslContext`` will speak.
    pub struct SslMethod;
    impl SslMethod {
        /// Support all versions of the TLS protocol.
        pub fn tls() -> SslMethod {
            panic!("Not implemented");
        }
    }

    /// Reference to an [`Ssl`].
    pub struct SslRef;
    impl SslRef {
        /// Like [`SslContextBuilder::set_verify`].
        pub fn set_verify(&mut self, _mode: SslVerifyMode) {
            panic!("Not implemented");
        }

        /// Returns the current cipher if the session is active.
        pub fn current_cipher(&self) -> Option<&SslCipherRef> {
            panic!("Not implemented");
        }

        /// Sets the host name to be sent to the server for Server Name Indication (SNI).
        pub fn set_hostname(&mut self, _hostname: &str) -> Result<(), ErrorStack> {
            panic!("Not implemented");
        }

        /// Returns the peer’s certificate, if present.
        pub fn peer_certificate(&self) -> Option<X509> {
            panic!("Not implemented");
        }

        /// Returns the certificate verification result.
        pub fn verify_result(&self) -> X509VerifyResult {
            panic!("Not implemented");
        }

        /// Returns a string describing the protocol version of the session.
        pub fn version_str(&self) -> &'static str {
            panic!("Not implemented");
        }

        /// Returns the protocol selected via Application Layer Protocol Negotiation (ALPN).
        pub fn selected_alpn_protocol(&self) -> Option<&[u8]> {
            panic!("Not implemented");
        }

        /// Returns the servername sent by the client via Server Name Indication (SNI).
        pub fn servername(&self, _type_: NameType) -> Option<&str> {
            panic!("Not implemented");
        }
    }

    /// Options controlling the behavior of certificate verification.
    pub struct SslVerifyMode;
    impl SslVerifyMode {
        /// Verifies that the peer’s certificate is trusted.
        pub const PEER: Self = Self {};

        /// Disables verification of the peer’s certificate.
        pub const NONE: Self = Self {};
    }

    /// An SSL/TLS protocol version.
    pub struct SslVersion;
    impl SslVersion {
        /// TLSv1.0
        pub const TLS1: SslVersion = Self {};

        /// TLSv1.2
        pub const TLS1_2: SslVersion = Self {};

        /// TLSv1.3
        pub const TLS1_3: SslVersion = Self {};
    }

    /// A standard implementation of protocol selection for Application Layer Protocol Negotiation
    /// (ALPN).
    pub fn select_next_proto<'a>(_server: &[u8], _client: &'a [u8]) -> Option<&'a [u8]> {
        panic!("Not implemented");
    }
}

pub mod ssl_sys {
    pub const X509_V_OK: i32 = 0;
    pub const X509_V_ERR_INVALID_CALL: i32 = 69;
}

pub mod error {
    use super::ssl::Error;

    /// Collection of [`Errors`] from OpenSSL.
    #[derive(Debug)]
    pub struct ErrorStack;
    impl_display!(ErrorStack);
    impl std::error::Error for ErrorStack {}
    impl ErrorStack {
        /// Returns the contents of the OpenSSL error stack.
        pub fn get() -> ErrorStack {
            panic!("Not implemented");
        }

        /// Returns the errors in the stack.
        pub fn errors(&self) -> &[Error] {
            panic!("Not implemented");
        }
    }
}

pub mod x509 {
    use super::asn1::{Asn1IntegerRef, Asn1StringRef, Asn1TimeRef};
    use super::error::ErrorStack;
    use super::hash::{DigestBytes, MessageDigest};
    use super::nid::Nid;

    /// An `X509` public key certificate.
    #[derive(Debug, Clone)]
    pub struct X509;
    impl_deref! {X509 => X509Ref}
    impl X509 {
        /// Deserializes a PEM-encoded X509 structure.
        pub fn from_pem(_pem: &[u8]) -> Result<X509, ErrorStack> {
            panic!("Not implemented");
        }
    }

    /// A type to destructure and examine an `X509Name`.
    pub struct X509NameEntries<'a> {
        marker: std::marker::PhantomData<&'a ()>,
    }
    impl<'a> Iterator for X509NameEntries<'a> {
        type Item = &'a X509NameEntryRef;
        fn next(&mut self) -> Option<&'a X509NameEntryRef> {
            panic!("Not implemented");
        }
    }

    /// Reference to `X509NameEntry`.
    pub struct X509NameEntryRef;
    impl X509NameEntryRef {
        pub fn data(&self) -> &Asn1StringRef {
            panic!("Not implemented");
        }
    }

    /// Reference to `X509Name`.
    pub struct X509NameRef;
    impl X509NameRef {
        /// Returns the name entries by the nid.
        pub fn entries_by_nid(&self, _nid: Nid) -> X509NameEntries<'_> {
            panic!("Not implemented");
        }
    }

    /// Reference to `X509`.
    pub struct X509Ref;
    impl X509Ref {
        /// Returns this certificate’s subject name.
        pub fn subject_name(&self) -> &X509NameRef {
            panic!("Not implemented");
        }

        /// Returns a digest of the DER representation of the certificate.
        pub fn digest(&self, _hash_type: MessageDigest) -> Result<DigestBytes, ErrorStack> {
            panic!("Not implemented");
        }

        /// Returns the certificate’s Not After validity period.
        pub fn not_after(&self) -> &Asn1TimeRef {
            panic!("Not implemented");
        }

        /// Returns this certificate’s serial number.
        pub fn serial_number(&self) -> &Asn1IntegerRef {
            panic!("Not implemented");
        }
    }

    /// The result of peer certificate verification.
    pub struct X509VerifyResult;
    impl X509VerifyResult {
        /// Return the integer representation of an `X509VerifyResult`.
        pub fn as_raw(&self) -> i32 {
            panic!("Not implemented");
        }
    }

    pub mod store {
        use super::super::error::ErrorStack;
        use super::X509;

        /// A builder type used to construct an `X509Store`.
        pub struct X509StoreBuilder;
        impl X509StoreBuilder {
            /// Returns a builder for a certificate store..
            pub fn new() -> Result<X509StoreBuilder, ErrorStack> {
                panic!("Not implemented");
            }

            /// Constructs the `X509Store`.
            pub fn build(self) -> X509Store {
                panic!("Not implemented");
            }

            /// Adds a certificate to the certificate store.
            pub fn add_cert(&mut self, _cert: X509) -> Result<(), ErrorStack> {
                panic!("Not implemented");
            }
        }

        /// A certificate store to hold trusted X509 certificates.
        pub struct X509Store;
        impl_deref! {X509Store => X509StoreRef}

        /// Reference to an `X509Store`.
        pub struct X509StoreRef;
    }

    pub mod verify {
        /// Reference to `X509VerifyParam`.
        pub struct X509VerifyParamRef;
    }
}

pub mod nid {
    /// A numerical identifier for an OpenSSL object.
    pub struct Nid;
    impl Nid {
        pub const COMMONNAME: Nid = Self {};
        pub const ORGANIZATIONNAME: Nid = Self {};
        pub const ORGANIZATIONALUNITNAME: Nid = Self {};
    }
}

pub mod pkey {
    use super::error::ErrorStack;

    /// A public or private key.
    #[derive(Clone)]
    pub struct PKey<T> {
        marker: std::marker::PhantomData<T>,
    }
    impl<T> std::ops::Deref for PKey<T> {
        type Target = PKeyRef<T>;
        fn deref(&self) -> &PKeyRef<T> {
            panic!("Not implemented");
        }
    }
    impl<T> std::ops::DerefMut for PKey<T> {
        fn deref_mut(&mut self) -> &mut PKeyRef<T> {
            panic!("Not implemented");
        }
    }
    impl PKey<Private> {
        pub fn private_key_from_pem(_pem: &[u8]) -> Result<PKey<Private>, ErrorStack> {
            panic!("Not implemented");
        }
    }

    /// Reference to `PKey`.
    pub struct PKeyRef<T> {
        marker: std::marker::PhantomData<T>,
    }

    /// A tag type indicating that a key has private components.
    #[derive(Clone)]
    pub enum Private {}
    unsafe impl HasPrivate for Private {}

    /// A trait indicating that a key has private components.
    pub unsafe trait HasPrivate {}
}

pub mod hash {
    /// A message digest algorithm.
    pub struct MessageDigest;
    impl MessageDigest {
        pub fn sha256() -> MessageDigest {
            panic!("Not implemented");
        }
    }

    /// The resulting bytes of a digest.
    pub struct DigestBytes;
    impl AsRef<[u8]> for DigestBytes {
        fn as_ref(&self) -> &[u8] {
            panic!("Not implemented");
        }
    }
}

pub mod asn1 {
    use super::bn::BigNum;
    use super::error::ErrorStack;

    /// A reference to an `Asn1Integer`.
    pub struct Asn1IntegerRef;
    impl Asn1IntegerRef {
        /// Converts the integer to a `BigNum`.
        pub fn to_bn(&self) -> Result<BigNum, ErrorStack> {
            panic!("Not implemented");
        }
    }

    /// A reference to an `Asn1String`.
    pub struct Asn1StringRef;
    impl Asn1StringRef {
        pub fn as_utf8(&self) -> Result<&str, ErrorStack> {
            panic!("Not implemented");
        }
    }

    /// Reference to an `Asn1Time`
    pub struct Asn1TimeRef;
    impl_display! {Asn1TimeRef}
}

pub mod bn {
    use super::error::ErrorStack;

    /// Dynamically sized large number implementation
    pub struct BigNum;
    impl BigNum {
        /// Returns a hexadecimal string representation of `self`.
        pub fn to_hex_str(&self) -> Result<&str, ErrorStack> {
            panic!("Not implemented");
        }
    }
}

pub mod ext {
    use super::error::ErrorStack;
    use super::pkey::{HasPrivate, PKeyRef};
    use super::ssl::{Ssl, SslAcceptor, SslRef};
    use super::x509::store::X509StoreRef;
    use super::x509::verify::X509VerifyParamRef;
    use super::x509::X509Ref;

    /// Add name as an additional reference identifier that can match the peer's certificate
    pub fn add_host(_verify_param: &mut X509VerifyParamRef, _host: &str) -> Result<(), ErrorStack> {
        panic!("Not implemented");
    }

    /// Set the verify cert store of `_ssl`
    pub fn ssl_set_verify_cert_store(
        _ssl: &mut SslRef,
        _cert_store: &X509StoreRef,
    ) -> Result<(), ErrorStack> {
        panic!("Not implemented");
    }

    /// Load the certificate into `_ssl`
    pub fn ssl_use_certificate(_ssl: &mut SslRef, _cert: &X509Ref) -> Result<(), ErrorStack> {
        panic!("Not implemented");
    }

    /// Load the private key into `_ssl`
    pub fn ssl_use_private_key<T>(_ssl: &mut SslRef, _key: &PKeyRef<T>) -> Result<(), ErrorStack>
    where
        T: HasPrivate,
    {
        panic!("Not implemented");
    }

    /// Clear the error stack
    pub fn clear_error_stack() {}

    /// Create a new [Ssl] from &[SslAcceptor]
    pub fn ssl_from_acceptor(_acceptor: &SslAcceptor) -> Result<Ssl, ErrorStack> {
        panic!("Not implemented");
    }

    /// Suspend the TLS handshake when a certificate is needed.
    pub fn suspend_when_need_ssl_cert(_ssl: &mut SslRef) {
        panic!("Not implemented");
    }

    /// Unblock a TLS handshake after the certificate is set.
    pub fn unblock_ssl_cert(_ssl: &mut SslRef) {
        panic!("Not implemented");
    }

    /// Whether the TLS error is SSL_ERROR_WANT_X509_LOOKUP
    pub fn is_suspended_for_cert(_error: &super::ssl::Error) -> bool {
        panic!("Not implemented");
    }

    /// Add the certificate into the cert chain of `_ssl`
    pub fn ssl_add_chain_cert(_ssl: &mut SslRef, _cert: &X509Ref) -> Result<(), ErrorStack> {
        panic!("Not implemented");
    }

    /// Set renegotiation
    pub fn ssl_set_renegotiate_mode_freely(_ssl: &mut SslRef) {}

    /// Set the curves/groups of `_ssl`
    pub fn ssl_set_groups_list(_ssl: &mut SslRef, _groups: &str) -> Result<(), ErrorStack> {
        panic!("Not implemented");
    }

    /// Sets whether a second keyshare to be sent in client hello when PQ is used.
    pub fn ssl_use_second_key_share(_ssl: &mut SslRef, _enabled: bool) {}

    /// Get a mutable SslRef ouf of SslRef, which is a missing functionality even when holding &mut SslStream
    /// # Safety
    pub unsafe fn ssl_mut(_ssl: &SslRef) -> &mut SslRef {
        panic!("Not implemented");
    }
}

pub mod tokio_ssl {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::error::ErrorStack;
    use super::ssl::{Error, Ssl, SslRef};

    /// A TLS session over a stream.
    #[derive(Debug)]
    pub struct SslStream<S> {
        marker: std::marker::PhantomData<S>,
    }
    impl<S> SslStream<S> {
        /// Creates a new `SslStream`.
        pub fn new(_ssl: Ssl, _stream: S) -> Result<Self, ErrorStack> {
            panic!("Not implemented");
        }

        /// Initiates a client-side TLS handshake.
        pub async fn connect(self: Pin<&mut Self>) -> Result<(), Error> {
            panic!("Not implemented");
        }

        /// Initiates a server-side TLS handshake.
        pub async fn accept(self: Pin<&mut Self>) -> Result<(), Error> {
            panic!("Not implemented");
        }

        /// Returns a shared reference to the `Ssl` object associated with this stream.
        pub fn ssl(&self) -> &SslRef {
            panic!("Not implemented");
        }

        /// Returns a shared reference to the underlying stream.
        pub fn get_ref(&self) -> &S {
            panic!("Not implemented");
        }

        /// Returns a mutable reference to the underlying stream.
        pub fn get_mut(&mut self) -> &mut S {
            panic!("Not implemented");
        }
    }
    impl<S> AsyncRead for SslStream<S>
    where
        S: AsyncRead + AsyncWrite,
    {
        fn poll_read(
            self: Pin<&mut Self>,
            _ctx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            panic!("Not implemented");
        }
    }
    impl<S> AsyncWrite for SslStream<S>
    where
        S: AsyncRead + AsyncWrite,
    {
        fn poll_write(
            self: Pin<&mut Self>,
            _ctx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            panic!("Not implemented");
        }

        fn poll_flush(self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            panic!("Not implemented");
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _ctx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            panic!("Not implemented");
        }
    }
}
