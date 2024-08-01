//! Windows specific functionality for calling the WinSock c api
//!
//! Implementations here are based on the implementation in the std library
//! https://github.com/rust-lang/rust/blob/84ac80f/library/std/src/sys_common/net.rs
//! https://github.com/rust-lang/rust/blob/84ac80f/library/std/src/sys/pal/windows/net.rs

use std::os::windows::io::RawSocket;
use std::{io, mem, net::SocketAddr};

use windows_sys::Win32::Networking::WinSock::{
    getpeername, getsockname, AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE,
    SOCKET,
};

pub(crate) fn peer_addr(raw_sock: RawSocket) -> io::Result<SocketAddr> {
    let mut storage = unsafe { mem::zeroed::<SOCKADDR_STORAGE>() };
    let mut addrlen = mem::size_of_val(&storage) as i32;

    unsafe {
        let res = getpeername(
            raw_sock as SOCKET,
            core::ptr::addr_of_mut!(storage) as *mut _,
            &mut addrlen,
        );
        if res != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    sockaddr_to_addr(&storage, addrlen as usize)
}
pub(crate) fn local_addr(raw_sock: RawSocket) -> io::Result<SocketAddr> {
    let mut storage = unsafe { mem::zeroed::<SOCKADDR_STORAGE>() };
    let mut addrlen = mem::size_of_val(&storage) as i32;

    unsafe {
        let res = getsockname(
            raw_sock as libc::SOCKET,
            core::ptr::addr_of_mut!(storage) as *mut _,
            &mut addrlen,
        );
        if res != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    sockaddr_to_addr(&storage, addrlen as usize)
}

fn sockaddr_to_addr(storage: &SOCKADDR_STORAGE, len: usize) -> io::Result<SocketAddr> {
    match storage.ss_family {
        AF_INET => {
            assert!(len >= mem::size_of::<SOCKADDR_IN>());
            Ok(SocketAddr::from(unsafe {
                let sockaddr = *(storage as *const _ as *const SOCKADDR_IN);
                (
                    sockaddr.sin_addr.S_un.S_addr.to_ne_bytes(),
                    sockaddr.sin_port.to_be(),
                )
            }))
        }
        AF_INET6 => {
            assert!(len >= mem::size_of::<SOCKADDR_IN6>());
            Ok(SocketAddr::from(unsafe {
                let sockaddr = *(storage as *const _ as *const SOCKADDR_IN6);
                (sockaddr.sin6_addr.u.Byte, sockaddr.sin6_port.to_be())
            }))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid argument",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawSocket;

    use crate::protocols::l4::{listener::Listener, stream::Stream};

    use super::*;

    async fn assert_listener_and_stream(addr: &str) {
        let tokio_listener = tokio::net::TcpListener::bind(addr).await.unwrap();

        let listener_local_addr = tokio_listener.local_addr().unwrap();

        let tokio_stream = tokio::net::TcpStream::connect(listener_local_addr)
            .await
            .unwrap();

        let stream_local_addr = tokio_stream.local_addr().unwrap();
        let stream_peer_addr = tokio_stream.peer_addr().unwrap();

        let stream: Stream = tokio_stream.into();
        let listener: Listener = tokio_listener.into();

        let raw_sock = listener.as_raw_socket();
        assert_eq!(listener_local_addr, local_addr(raw_sock).unwrap());

        let raw_sock = stream.as_raw_socket();
        assert_eq!(stream_peer_addr, peer_addr(raw_sock).unwrap());
        assert_eq!(stream_local_addr, local_addr(raw_sock).unwrap());
    }

    #[tokio::test]
    async fn get_v4_addrs_from_raw_socket() {
        assert_listener_and_stream("127.0.0.1:0").await
    }
    #[tokio::test]
    async fn get_v6_addrs_from_raw_socket() {
        assert_listener_and_stream("[::1]:0").await
    }
}
