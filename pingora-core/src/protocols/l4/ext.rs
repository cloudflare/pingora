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

//! Extensions to the regular TCP APIs

#![allow(non_camel_case_types)]

use libc::socklen_t;
#[cfg(target_os = "linux")]
use libc::{c_int, c_void};
use pingora_error::{Error, ErrorType::*, OrErr, Result};
use std::io::{self, ErrorKind};
use std::mem;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;
use tokio::net::{TcpSocket, TcpStream, UnixStream};

/// The (copy of) the kernel struct tcp_info returns
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct TCP_INFO {
    tcpi_state: u8,
    tcpi_ca_state: u8,
    tcpi_retransmits: u8,
    tcpi_probes: u8,
    tcpi_backoff: u8,
    tcpi_options: u8,
    tcpi_snd_wscale_4_rcv_wscale_4: u8,
    tcpi_delivery_rate_app_limited: u8,
    tcpi_rto: u32,
    tcpi_ato: u32,
    tcpi_snd_mss: u32,
    tcpi_rcv_mss: u32,
    tcpi_unacked: u32,
    tcpi_sacked: u32,
    tcpi_lost: u32,
    tcpi_retrans: u32,
    tcpi_fackets: u32,
    tcpi_last_data_sent: u32,
    tcpi_last_ack_sent: u32,
    tcpi_last_data_recv: u32,
    tcpi_last_ack_recv: u32,
    tcpi_pmtu: u32,
    tcpi_rcv_ssthresh: u32,
    pub tcpi_rtt: u32,
    tcpi_rttvar: u32,
    /* uncomment these field if needed
    tcpi_snd_ssthresh: u32,
    tcpi_snd_cwnd: u32,
    tcpi_advmss: u32,
    tcpi_reordering: u32,
    tcpi_rcv_rtt: u32,
    tcpi_rcv_space: u32,
    tcpi_total_retrans: u32,
    tcpi_pacing_rate: u64,
    tcpi_max_pacing_rate: u64,
    tcpi_bytes_acked: u64,
    tcpi_bytes_received: u64,
    tcpi_segs_out: u32,
    tcpi_segs_in: u32,
    tcpi_notsent_bytes: u32,
    tcpi_min_rtt: u32,
    tcpi_data_segs_in: u32,
    tcpi_data_segs_out: u32,
    tcpi_delivery_rate: u64,
    */
    /* and more, see include/linux/tcp.h */
}

impl TCP_INFO {
    /// Create a new zeroed out [`TCP_INFO`]
    pub unsafe fn new() -> Self {
        mem::zeroed()
    }

    /// Return the size of [`TCP_INFO`]
    pub fn len() -> socklen_t {
        mem::size_of::<Self>() as socklen_t
    }
}

#[cfg(target_os = "linux")]
fn set_opt<T: Copy>(sock: c_int, opt: c_int, val: c_int, payload: T) -> io::Result<()> {
    unsafe {
        let payload = &payload as *const T as *const c_void;
        cvt_linux_error(libc::setsockopt(
            sock,
            opt,
            val,
            payload as *const _,
            mem::size_of::<T>() as socklen_t,
        ))?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn get_opt<T>(
    sock: c_int,
    opt: c_int,
    val: c_int,
    payload: &mut T,
    size: &mut socklen_t,
) -> io::Result<()> {
    unsafe {
        let payload = payload as *mut T as *mut c_void;
        cvt_linux_error(libc::getsockopt(sock, opt, val, payload as *mut _, size))?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn cvt_linux_error(t: i32) -> io::Result<i32> {
    if t == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(t)
    }
}

#[cfg(target_os = "linux")]
fn ip_bind_addr_no_port(fd: RawFd, val: bool) -> io::Result<()> {
    const IP_BIND_ADDRESS_NO_PORT: i32 = 24;

    set_opt(fd, libc::IPPROTO_IP, IP_BIND_ADDRESS_NO_PORT, val as c_int)
}

#[cfg(not(target_os = "linux"))]
fn ip_bind_addr_no_port(_fd: RawFd, _val: bool) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_so_keepalive(fd: RawFd, val: bool) -> io::Result<()> {
    set_opt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, val as c_int)
}

#[cfg(target_os = "linux")]
fn set_so_keepalive_idle(fd: RawFd, val: Duration) -> io::Result<()> {
    set_opt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPIDLE,
        val.as_secs() as c_int, // only the seconds part of val is used
    )
}

#[cfg(target_os = "linux")]
fn set_so_keepalive_interval(fd: RawFd, val: Duration) -> io::Result<()> {
    set_opt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPINTVL,
        val.as_secs() as c_int, // only the seconds part of val is used
    )
}

#[cfg(target_os = "linux")]
fn set_so_keepalive_count(fd: RawFd, val: usize) -> io::Result<()> {
    set_opt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT, val as c_int)
}

#[cfg(target_os = "linux")]
fn set_keepalive(fd: RawFd, ka: &TcpKeepalive) -> io::Result<()> {
    set_so_keepalive(fd, true)?;
    set_so_keepalive_idle(fd, ka.idle)?;
    set_so_keepalive_interval(fd, ka.interval)?;
    set_so_keepalive_count(fd, ka.count)
}

#[cfg(not(target_os = "linux"))]
fn set_keepalive(_fd: RawFd, _ka: &TcpKeepalive) -> io::Result<()> {
    Ok(())
}

/// Get the kernel TCP_INFO for the given FD.
#[cfg(target_os = "linux")]
pub fn get_tcp_info(fd: RawFd) -> io::Result<TCP_INFO> {
    let mut tcp_info = unsafe { TCP_INFO::new() };
    let mut data_len: socklen_t = TCP_INFO::len();
    get_opt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_INFO,
        &mut tcp_info,
        &mut data_len,
    )?;
    if data_len != TCP_INFO::len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "TCP_INFO struct size mismatch",
        ));
    }
    Ok(tcp_info)
}

#[cfg(not(target_os = "linux"))]
pub fn get_tcp_info(_fd: RawFd) -> io::Result<TCP_INFO> {
    Ok(unsafe { TCP_INFO::new() })
}

/*
 * this extension is needed until the following are addressed
 * https://github.com/tokio-rs/tokio/issues/1543
 * https://github.com/tokio-rs/mio/issues/1257
 * https://github.com/tokio-rs/mio/issues/1211
 */
/// connect() to the given address while optionally bind to the specific source address
///
/// `IP_BIND_ADDRESS_NO_PORT` is used.
pub async fn connect(addr: &SocketAddr, bind_to: Option<&SocketAddr>) -> Result<TcpStream> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
    .or_err(SocketError, "failed to create socket")?;

    if cfg!(target_os = "linux") {
        ip_bind_addr_no_port(socket.as_raw_fd(), true)
            .or_err(SocketError, "failed to set socket opts")?;

        if let Some(baddr) = bind_to {
            socket
                .bind(*baddr)
                .or_err_with(BindError, || format!("failed to bind to socket {}", *baddr))?;
        };
    }
    // TODO: add support for bind on other platforms

    socket
        .connect(*addr)
        .await
        .map_err(|e| wrap_os_connect_error(e, format!("Fail to connect to {}", *addr)))
}

/// connect() to the given Unix domain socket
pub async fn connect_uds(path: &std::path::Path) -> Result<UnixStream> {
    UnixStream::connect(path)
        .await
        .map_err(|e| wrap_os_connect_error(e, format!("Fail to connect to {}", path.display())))
}

fn wrap_os_connect_error(e: std::io::Error, context: String) -> Box<Error> {
    match e.kind() {
        ErrorKind::ConnectionRefused => Error::because(ConnectRefused, context, e),
        ErrorKind::TimedOut => Error::because(ConnectTimedout, context, e),
        ErrorKind::PermissionDenied | ErrorKind::AddrInUse | ErrorKind::AddrNotAvailable => {
            Error::because(InternalError, context, e)
        }
        _ => match e.raw_os_error() {
            Some(code) => match code {
                libc::ENETUNREACH | libc::EHOSTUNREACH => {
                    Error::because(ConnectNoRoute, context, e)
                }
                _ => Error::because(ConnectError, context, e),
            },
            None => Error::because(ConnectError, context, e),
        },
    }
}

/// The configuration for TCP keepalive
#[derive(Clone, Debug)]
pub struct TcpKeepalive {
    /// The time a connection needs to be idle before TCP begins sending out keep-alive probes.
    pub idle: Duration,
    /// The number of seconds between TCP keep-alive probes.
    pub interval: Duration,
    /// The maximum number of TCP keep-alive probes to send before giving up and killing the connection
    pub count: usize,
}

impl std::fmt::Display for TcpKeepalive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}/{:?}/{}", self.idle, self.interval, self.count)
    }
}

/// Apply the given TCP keepalive settings to the given connection
pub fn set_tcp_keepalive(stream: &TcpStream, ka: &TcpKeepalive) -> Result<()> {
    let fd = stream.as_raw_fd();
    // TODO: check localhost or if keepalive is already set
    set_keepalive(fd, ka).or_err(ConnectError, "failed to set keepalive")
}
