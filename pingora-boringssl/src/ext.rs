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

//! the extended functionalities that are yet exposed via the [`boring`] APIs

use boring::error::ErrorStack;
use boring::ex_data::Index;
use boring::pkey::{HasPrivate, PKeyRef};
use boring::ssl::{Ssl, SslAcceptor, SslRef};
use boring::x509::store::X509StoreRef;
use boring::x509::verify::X509VerifyParamRef;
use boring::x509::X509Ref;
use foreign_types_shared::ForeignTypeRef;
use libc::*;
use std::ffi::CString;
use std::sync::OnceLock;

fn cvt(r: c_int) -> Result<c_int, ErrorStack> {
    if r != 1 {
        Err(ErrorStack::get())
    } else {
        Ok(r)
    }
}

/// Add name as an additional reference identifier that can match the peer's certificate
///
/// See [X509_VERIFY_PARAM_set1_host](https://www.openssl.org/docs/man3.1/man3/X509_VERIFY_PARAM_set1_host.html).
pub fn add_host(verify_param: &mut X509VerifyParamRef, host: &str) -> Result<(), ErrorStack> {
    if host.is_empty() {
        return Ok(());
    }
    unsafe {
        cvt(boring_sys::X509_VERIFY_PARAM_add1_host(
            verify_param.as_ptr(),
            host.as_ptr() as *const _,
            host.len(),
        ))
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
        cvt(boring_sys::SSL_set1_verify_cert_store(
            ssl.as_ptr(),
            cert_store.as_ptr(),
        ))?;
    }
    Ok(())
}

/// Load the certificate into `ssl`
///
/// See [SSL_use_certificate](https://www.openssl.org/docs/man1.1.1/man3/SSL_use_certificate.html).
pub fn ssl_use_certificate(ssl: &mut SslRef, cert: &X509Ref) -> Result<(), ErrorStack> {
    unsafe {
        cvt(boring_sys::SSL_use_certificate(ssl.as_ptr(), cert.as_ptr()))?;
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
        cvt(boring_sys::SSL_use_PrivateKey(ssl.as_ptr(), key.as_ptr()))?;
    }
    Ok(())
}

/// Add the certificate into the cert chain of `ssl`
///
/// See [SSL_add1_chain_cert](https://www.openssl.org/docs/man1.1.1/man3/SSL_add1_chain_cert.html)
pub fn ssl_add_chain_cert(ssl: &mut SslRef, cert: &X509Ref) -> Result<(), ErrorStack> {
    unsafe {
        cvt(boring_sys::SSL_add1_chain_cert(ssl.as_ptr(), cert.as_ptr()))?;
    }
    Ok(())
}

/// Set renegotiation
///
/// This function is specific to BoringSSL
/// See <https://commondatastorage.googleapis.com/chromium-boringssl-docs/ssl.h.html#SSL_set_renegotiate_mode>
pub fn ssl_set_renegotiate_mode_freely(ssl: &mut SslRef) {
    unsafe {
        boring_sys::SSL_set_renegotiate_mode(
            ssl.as_ptr(),
            boring_sys::ssl_renegotiate_mode_t::ssl_renegotiate_freely,
        );
    }
}

/// Set the curves/groups of `ssl`
///
/// See [set_groups_list](https://www.openssl.org/docs/manmaster/man3/SSL_CTX_set1_curves.html).
pub fn ssl_set_groups_list(ssl: &mut SslRef, groups: &str) -> Result<(), ErrorStack> {
    let groups = CString::new(groups).unwrap();
    unsafe {
        // somehow SSL_set1_groups_list doesn't exist but SSL_set1_curves_list means the same anyways
        cvt(boring_sys::SSL_set1_curves_list(
            ssl.as_ptr(),
            groups.as_ptr(),
        ))?;
    }
    Ok(())
}

/// Set's whether a second keyshare to be sent in client hello when PQ is used.
///
/// Default is true. When `true`, the first PQ (if any) and none-PQ keyshares are sent.
/// When `false`, only the first configured keyshares are sent.
#[cfg(feature = "pq_use_second_keyshare")]
pub fn ssl_use_second_key_share(ssl: &mut SslRef, enabled: bool) {
    unsafe { boring_sys::SSL_use_second_keyshare(ssl.as_ptr(), enabled as _) }
}
#[cfg(not(feature = "pq_use_second_keyshare"))]
pub fn ssl_use_second_key_share(_ssl: &mut SslRef, _enabled: bool) {}

/// Clear the error stack
///
/// SSL calls should check and clear the BoringSSL error stack. But some calls fail to do so.
/// This causes the next unrelated SSL call to fail due to the leftover errors. This function allows
/// the caller to clear the error stack before performing SSL calls to avoid this issue.
pub fn clear_error_stack() {
    let _ = ErrorStack::get();
}

/// Export keying material from a TLS connection
///
/// Derives keying material for application use in accordance with RFC 5705.
///
/// See [SSL_export_keying_material](https://commondatastorage.googleapis.com/chromium-boringssl-docs/ssl.h.html#SSL_export_keying_material).
pub fn ssl_export_keying_material(
    ssl: &SslRef,
    out: &mut [u8],
    label: &str,
    context: Option<&[u8]>,
) -> Result<(), ErrorStack> {
    ssl.export_keying_material(out, label, context)
}

/// Create a new [Ssl] from &[SslAcceptor]
///
/// This function is needed because [Ssl::new()] doesn't take `&SslContextRef` like openssl-rs
pub fn ssl_from_acceptor(acceptor: &SslAcceptor) -> Result<Ssl, ErrorStack> {
    Ssl::new_from_ref(acceptor.context())
}

/// Suspend the TLS handshake when a certificate is needed.
///
/// This function will cause tls handshake to pause and return the error: SSL_ERROR_WANT_X509_LOOKUP.
/// The caller should set the certificate and then call [unblock_ssl_cert()] before continue the
/// handshake on the tls connection.
pub fn suspend_when_need_ssl_cert(ssl: &mut SslRef) {
    unsafe {
        boring_sys::SSL_set_cert_cb(ssl.as_ptr(), Some(raw_cert_block), std::ptr::null_mut());
    }
}

/// Unblock a TLS handshake after the certificate is set.
///
/// The user should continue to call tls handshake after this function is called.
pub fn unblock_ssl_cert(ssl: &mut SslRef) {
    unsafe {
        boring_sys::SSL_set_cert_cb(ssl.as_ptr(), None, std::ptr::null_mut());
    }
}

// Just block the handshake
extern "C" fn raw_cert_block(_ssl: *mut boring_sys::SSL, _arg: *mut c_void) -> c_int {
    -1
}

/// Whether the TLS error is SSL_ERROR_WANT_X509_LOOKUP
pub fn is_suspended_for_cert(error: &boring::ssl::Error) -> bool {
    error.code().as_raw() == boring_sys::SSL_ERROR_WANT_X509_LOOKUP
}

#[allow(clippy::mut_from_ref)]
/// Get a mutable SslRef ouf of SslRef. which is a missing functionality for certain SslStream
/// # Safety
/// the caller needs to make sure that they hold a &mut SslRef
pub unsafe fn ssl_mut(ssl: &SslRef) -> &mut SslRef {
    unsafe { SslRef::from_ptr_mut(ssl.as_ptr()) }
}

// The downstream socket identifier, matching pingora-core's `UniqueIDType`
// (`RawFd` on unix, `RawSocket`-as-usize on Windows). Mirrored here because
// this crate sits below pingora-core and cannot import that alias; the two
// resolve to the same concrete type, so `handshake_with_callback` passes
// `io.id()` straight through with no cast.
#[cfg(unix)]
type DownstreamFd = i32;
#[cfg(windows)]
type DownstreamFd = usize;

// Carry the downstream socket fd across the handshake so an async certificate
// callback can recover the connection's identity via getsockopt. The fd is
// stashed on the SSL by `handshake_with_callback` (see
// pingora-core boringssl_openssl/server.rs) just before the callback runs, and
// read back by the callback.
fn downstream_fd_index() -> Index<Ssl, DownstreamFd> {
    // One process-wide ex_data slot, shared between setter and getter. A second
    // `new_ex_index()` would allocate a *different* slot and never match.
    static IDX: OnceLock<Index<Ssl, DownstreamFd>> = OnceLock::new();
    *IDX.get_or_init(|| Ssl::new_ex_index().expect("alloc ssl ex_index for downstream fd"))
}

/// Stash the downstream socket fd on the SSL so an async certificate callback
/// can recover the connection's identity (e.g. getsockopt) during the handshake.
pub fn set_downstream_fd(ssl: &mut SslRef, fd: DownstreamFd) {
    ssl.set_ex_data(downstream_fd_index(), fd);
}

/// Recover the downstream fd stashed by [`set_downstream_fd`], if any.
pub fn get_downstream_fd(ssl: &SslRef) -> Option<DownstreamFd> {
    ssl.ex_data(downstream_fd_index()).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use boring::ssl::{SslContextBuilder, SslMethod};

    #[test]
    fn test_downstream_fd_round_trips() {
        let ctx_builder = SslContextBuilder::new(SslMethod::tls()).unwrap();
        let ssl = Ssl::new(&ctx_builder.build()).unwrap();
        let ssl_ref = unsafe { ssl_mut(&ssl) };

        // Nothing stashed yet.
        assert_eq!(get_downstream_fd(ssl_ref), None);

        // A stashed fd reads back through the shared ex_data slot.
        set_downstream_fd(ssl_ref, 42);
        assert_eq!(get_downstream_fd(ssl_ref), Some(42));

        // Overwriting replaces the previous value.
        set_downstream_fd(ssl_ref, 7);
        assert_eq!(get_downstream_fd(ssl_ref), Some(7));
    }
}
