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

use foreign_types::ForeignTypeRef;
use libc::*;
use openssl::error::ErrorStack;
use openssl::pkey::{HasPrivate, PKeyRef};
use openssl::ssl::{Ssl, SslAcceptor, SslRef};
use openssl::x509::store::X509StoreRef;
use openssl::x509::verify::X509VerifyParamRef;
use openssl::x509::X509Ref;
use openssl_sys::{
    SSL_ctrl, EVP_PKEY, SSL, SSL_CTRL_SET_GROUPS_LIST, SSL_CTRL_SET_VERIFY_CERT_STORE, X509,
    X509_VERIFY_PARAM,
};
use std::ffi::CString;
use std::os::raw;

fn cvt(r: c_long) -> Result<c_long, ErrorStack> {
    if r != 1 {
        Err(ErrorStack::get())
    } else {
        Ok(r)
    }
}

extern "C" {
    pub fn X509_VERIFY_PARAM_add1_host(
        param: *mut X509_VERIFY_PARAM,
        name: *const c_char,
        namelen: size_t,
    ) -> c_int;

    pub fn SSL_use_certificate(ssl: *mut SSL, cert: *mut X509) -> c_int;
    pub fn SSL_use_PrivateKey(ssl: *mut SSL, key: *mut EVP_PKEY) -> c_int;

    pub fn SSL_set_cert_cb(
        ssl: *mut SSL,
        cb: ::std::option::Option<
            unsafe extern "C" fn(ssl: *mut SSL, arg: *mut raw::c_void) -> raw::c_int,
        >,
        arg: *mut raw::c_void,
    );
}

/// Add name as an additional reference identifier that can match the peer's certificate
///
/// See [X509_VERIFY_PARAM_set1_host](https://www.openssl.org/docs/man3.1/man3/X509_VERIFY_PARAM_set1_host.html).
pub fn add_host(verify_param: &mut X509VerifyParamRef, host: &str) -> Result<(), ErrorStack> {
    if host.is_empty() {
        return Ok(());
    }
    unsafe {
        cvt(X509_VERIFY_PARAM_add1_host(
            verify_param.as_ptr(),
            host.as_ptr() as *const c_char,
            host.len(),
        ) as c_long)
        .map(|_| ())
    }
}

/// Set the verify cert store of `ssl`
///
/// See [SSL_set1_verify_cert_store](https://www.openssl.org/docs/man1.1.1/man3/SSL_set1_verify_cert_store.html).
pub fn ssl_set_verify_cert_store(
    ssl: &mut SslRef,
    cert_store: &X509StoreRef,
) -> Result<(), ErrorStack> {
    unsafe {
        cvt(SSL_ctrl(
            ssl.as_ptr(),
            SSL_CTRL_SET_VERIFY_CERT_STORE,
            1, // increase the ref count of X509Store so that ssl_ctx can outlive X509StoreRef
            cert_store.as_ptr() as *mut c_void,
        ))?;
    }
    Ok(())
}

/// Load the certificate into `ssl`
///
/// See [SSL_use_certificate](https://www.openssl.org/docs/man1.1.1/man3/SSL_use_certificate.html).
pub fn ssl_use_certificate(ssl: &mut SslRef, cert: &X509Ref) -> Result<(), ErrorStack> {
    unsafe {
        cvt(SSL_use_certificate(ssl.as_ptr() as *mut SSL, cert.as_ptr() as *mut X509) as c_long)?;
    }
    Ok(())
}

/// Load the private key into `ssl`
///
/// See [SSL_use_certificate](https://www.openssl.org/docs/man1.1.1/man3/SSL_use_PrivateKey.html).
pub fn ssl_use_private_key<T>(ssl: &mut SslRef, key: &PKeyRef<T>) -> Result<(), ErrorStack>
where
    T: HasPrivate,
{
    unsafe {
        cvt(SSL_use_PrivateKey(ssl.as_ptr() as *mut SSL, key.as_ptr() as *mut EVP_PKEY) as c_long)?;
    }
    Ok(())
}

/// Add the certificate into the cert chain of `ssl`
///
/// See [SSL_add1_chain_cert](https://www.openssl.org/docs/man1.1.1/man3/SSL_add1_chain_cert.html)
pub fn ssl_add_chain_cert(ssl: &mut SslRef, cert: &X509Ref) -> Result<(), ErrorStack> {
    const SSL_CTRL_CHAIN_CERT: i32 = 89;
    unsafe {
        cvt(SSL_ctrl(
            ssl.as_ptr(),
            SSL_CTRL_CHAIN_CERT,
            1, // increase the ref count of X509 so that ssl can outlive X509StoreRef
            cert.as_ptr() as *mut c_void,
        ))?;
    }
    Ok(())
}

/// Set renegotiation
///
/// This function is specific to BoringSSL. This function is noop for OpenSSL.
pub fn ssl_set_renegotiate_mode_freely(_ssl: &mut SslRef) {}

/// Set the curves/groups of `ssl`
///
/// See [set_groups_list](https://www.openssl.org/docs/manmaster/man3/SSL_CTX_set1_curves.html).
pub fn ssl_set_groups_list(ssl: &mut SslRef, groups: &str) -> Result<(), ErrorStack> {
    if groups.contains('\0') {
        return Err(ErrorStack::get());
    }
    let groups = CString::new(groups).map_err(|_| ErrorStack::get())?;
    unsafe {
        cvt(SSL_ctrl(
            ssl.as_ptr(),
            SSL_CTRL_SET_GROUPS_LIST,
            0,
            groups.as_ptr() as *mut c_void,
        ))?;
    }
    Ok(())
}

/// Set's whether a second keyshare to be sent in client hello when PQ is used.
///
/// This function is specific to BoringSSL. This function is noop for OpenSSL.
pub fn ssl_use_second_key_share(_ssl: &mut SslRef, _enabled: bool) {}

/// Clear the error stack
///
/// SSL calls should check and clear the OpenSSL error stack. But some calls fail to do so.
/// This causes the next unrelated SSL call to fail due to the leftover errors. This function allows
/// caller to clear the error stack before performing SSL calls to avoid this issue.
pub fn clear_error_stack() {
    let _ = ErrorStack::get();
}

/// Create a new [Ssl] from &[SslAcceptor]
///
/// this function is to unify the interface between this crate and [`pingora-boringssl`](https://docs.rs/pingora-boringssl)
pub fn ssl_from_acceptor(acceptor: &SslAcceptor) -> Result<Ssl, ErrorStack> {
    Ssl::new(acceptor.context())
}

/// Suspend the TLS handshake when a certificate is needed.
///
/// This function will cause tls handshake to pause and return the error: SSL_ERROR_WANT_X509_LOOKUP.
/// The caller should set the certificate and then call [unblock_ssl_cert()] before continue the
/// handshake on the tls connection.
pub fn suspend_when_need_ssl_cert(ssl: &mut SslRef) {
    unsafe {
        SSL_set_cert_cb(ssl.as_ptr(), Some(raw_cert_block), std::ptr::null_mut());
    }
}

/// Unblock a TLS handshake after the certificate is set.
///
/// The user should continue to call tls handshake after this function is called.
pub fn unblock_ssl_cert(ssl: &mut SslRef) {
    unsafe {
        SSL_set_cert_cb(ssl.as_ptr(), None, std::ptr::null_mut());
    }
}

// Just block the handshake
extern "C" fn raw_cert_block(_ssl: *mut openssl_sys::SSL, _arg: *mut c_void) -> c_int {
    -1
}

/// Whether the TLS error is SSL_ERROR_WANT_X509_LOOKUP
pub fn is_suspended_for_cert(error: &openssl::ssl::Error) -> bool {
    error.code().as_raw() == openssl_sys::SSL_ERROR_WANT_X509_LOOKUP
}

#[allow(clippy::mut_from_ref)]
/// Get a mutable SslRef ouf of SslRef, which is a missing functionality even when holding &mut SslStream
/// # Safety
/// the caller needs to make sure that they hold a &mut SslStream (or other types of mutable ref to the Ssl)
pub unsafe fn ssl_mut(ssl: &SslRef) -> &mut SslRef {
    SslRef::from_ptr_mut(ssl.as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::ssl::{SslContextBuilder, SslMethod};

    #[test]
    fn test_ssl_set_groups_list() {
        let ctx_builder = SslContextBuilder::new(SslMethod::tls()).unwrap();
        let ssl = Ssl::new(&ctx_builder.build()).unwrap();
        let ssl_ref = unsafe { ssl_mut(&ssl) };

        // Valid input
        assert!(ssl_set_groups_list(ssl_ref, "P-256:P-384").is_ok());

        // Invalid input (contains null byte)
        assert!(ssl_set_groups_list(ssl_ref, "P-256\0P-384").is_err());
    }
}
