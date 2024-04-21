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

#[cfg(target_os = "linux")]
use log::{debug, error, warn};
use nix::errno::Errno;
#[cfg(target_os = "linux")]
use nix::sys::socket::{self, AddressFamily, RecvMsg, SockFlag, SockType, UnixAddr};
#[cfg(target_os = "linux")]
use nix::sys::stat;
use nix::{Error, NixPath};
use std::collections::HashMap;
use std::io::Write;
#[cfg(target_os = "linux")]
use std::io::{IoSlice, IoSliceMut};
use std::os::unix::io::RawFd;
#[cfg(target_os = "linux")]
use std::{thread, time};

// Utilities to transfer file descriptors between sockets, e.g. during graceful upgrades.

/// Container for open file descriptors and their associated bind addresses.
pub struct Fds {
    map: HashMap<String, RawFd>,
}

impl Fds {
    pub fn new() -> Self {
        Fds {
            map: HashMap::new(),
        }
    }

    pub fn add(&mut self, bind: String, fd: RawFd) {
        self.map.insert(bind, fd);
    }

    pub fn get(&self, bind: &str) -> Option<&RawFd> {
        self.map.get(bind)
    }

    pub fn serialize(&self) -> (Vec<String>, Vec<RawFd>) {
        self.map.iter().map(|(key, val)| (key.clone(), val)).unzip()
    }

    pub fn deserialize(&mut self, binds: Vec<String>, fds: Vec<RawFd>) {
        assert_eq!(binds.len(), fds.len());
        for (bind, fd) in binds.into_iter().zip(fds) {
            self.map.insert(bind, fd);
        }
    }

    pub fn send_to_sock<P>(&self, path: &P) -> Result<usize, Error>
    where
        P: ?Sized + NixPath + std::fmt::Display,
    {
        let (vec_key, vec_fds) = self.serialize();
        let mut ser_buf: [u8; 2048] = [0; 2048];
        let ser_key_size = serialize_vec_string(&vec_key, &mut ser_buf);
        send_fds_to(vec_fds, &ser_buf[..ser_key_size], path)
    }

    pub fn get_from_sock<P>(&mut self, path: &P) -> Result<(), Error>
    where
        P: ?Sized + NixPath + std::fmt::Display,
    {
        let mut de_buf: [u8; 2048] = [0; 2048];
        let (fds, bytes) = get_fds_from(path, &mut de_buf)?;
        let keys = deserialize_vec_string(&de_buf[..bytes])?;
        self.deserialize(keys, fds);
        Ok(())
    }
}

fn serialize_vec_string(vec_string: &[String], mut buf: &mut [u8]) -> usize {
    // There are many ways to do this. Serde is probably the way to go
    // But let's start with something simple: space separated strings
    let joined = vec_string.join(" ");
    // TODO: check the buf is large enough
    buf.write(joined.as_bytes()).unwrap()
}

fn deserialize_vec_string(buf: &[u8]) -> Result<Vec<String>, Error> {
    let joined = std::str::from_utf8(buf).map_err(|_| Error::EINVAL)?;
    Ok(joined.split_ascii_whitespace().map(String::from).collect())
}

#[cfg(target_os = "linux")]
pub fn get_fds_from<P>(path: &P, payload: &mut [u8]) -> Result<(Vec<RawFd>, usize), Error>
where
    P: ?Sized + NixPath + std::fmt::Display,
{
    const MAX_FDS: usize = 32;

    let listen_fd = socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK,
        None,
    )
    .unwrap();
    let unix_addr = UnixAddr::new(path).unwrap();
    // clean up old sock
    match nix::unistd::unlink(path) {
        Ok(()) => {
            debug!("unlink {} done", path);
        }
        Err(e) => {
            // Normal if file does not exist
            debug!("unlink {} failed: {}", path, e);
            // TODO: warn if exist but not able to unlink
        }
    };
    socket::bind(listen_fd, &unix_addr).unwrap();

    /* sock is created before we change user, need to give permission to all */
    stat::fchmodat(
        None,
        path,
        stat::Mode::all(),
        stat::FchmodatFlags::FollowSymlink,
    )
    .unwrap();

    socket::listen(listen_fd, 8).unwrap();

    let fd = match accept_with_retry(listen_fd) {
        Ok(fd) => fd,
        Err(e) => {
            error!("Giving up reading socket from: {path}, error: {e:?}");
            //cleanup
            if nix::unistd::close(listen_fd).is_ok() {
                nix::unistd::unlink(path).unwrap();
            }
            return Err(e);
        }
    };

    let mut io_vec = [IoSliceMut::new(payload); 1];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; MAX_FDS]);
    let msg: RecvMsg<UnixAddr> = socket::recvmsg(
        fd,
        &mut io_vec,
        Some(&mut cmsg_buf),
        socket::MsgFlags::empty(),
    )
    .unwrap();

    let mut fds: Vec<RawFd> = Vec::new();
    for cmsg in msg.cmsgs() {
        if let socket::ControlMessageOwned::ScmRights(mut vec_fds) = cmsg {
            fds.append(&mut vec_fds)
        } else {
            warn!("Unexpected control messages: {cmsg:?}")
        }
    }

    //cleanup
    if nix::unistd::close(listen_fd).is_ok() {
        nix::unistd::unlink(path).unwrap();
    }

    Ok((fds, msg.bytes))
}

#[cfg(not(target_os = "linux"))]
pub fn get_fds_from<P>(_path: &P, _payload: &mut [u8]) -> Result<(Vec<RawFd>, usize), Error>
where
    P: ?Sized + NixPath + std::fmt::Display,
{
    Err(Errno::ECONNREFUSED)
}

#[cfg(target_os = "linux")]
const MAX_RETRY: usize = 5;
#[cfg(target_os = "linux")]
const RETRY_INTERVAL: time::Duration = time::Duration::from_secs(1);

#[cfg(target_os = "linux")]
fn accept_with_retry(listen_fd: i32) -> Result<i32, Error> {
    let mut retried = 0;
    loop {
        match socket::accept(listen_fd) {
            Ok(fd) => return Ok(fd),
            Err(e) => {
                if retried > MAX_RETRY {
                    return Err(e);
                }
                match e {
                    Errno::EAGAIN => {
                        error!(
                            "No incoming socket transfer, sleep {RETRY_INTERVAL:?} and try again"
                        );
                        retried += 1;
                        thread::sleep(RETRY_INTERVAL);
                    }
                    _ => {
                        error!("Error accepting socket transfer: {e}");
                        return Err(e);
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub fn send_fds_to<P>(fds: Vec<RawFd>, payload: &[u8], path: &P) -> Result<usize, Error>
where
    P: ?Sized + NixPath + std::fmt::Display,
{
    const MAX_NONBLOCKING_POLLS: usize = 20;
    const NONBLOCKING_POLL_INTERVAL: time::Duration = time::Duration::from_millis(500);

    let send_fd = socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK,
        None,
    )?;
    let unix_addr = UnixAddr::new(path)?;
    let mut retried = 0;
    let mut nonblocking_polls = 0;

    let conn_result: Result<usize, Error> = loop {
        match socket::connect(send_fd, &unix_addr) {
            Ok(_) => break Ok(0),
            Err(e) => match e {
                /* If the new process hasn't created the upgrade sock we'll get an ENOENT.
                ECONNREFUSED may happen if the sock wasn't cleaned up
                and the old process tries sending before the new one is listening.
                EACCES may happen if connect() happen before the correct permission is set */
                Errno::ENOENT | Errno::ECONNREFUSED | Errno::EACCES => {
                    /*the server is not ready yet*/
                    retried += 1;
                    if retried > MAX_RETRY {
                        error!(
                            "Max retry: {} reached. Giving up sending socket to: {}, error: {:?}",
                            MAX_RETRY, path, e
                        );
                        break Err(e);
                    }
                    warn!("server not ready, will try again in {RETRY_INTERVAL:?}");
                    thread::sleep(RETRY_INTERVAL);
                }
                /* handle nonblocking IO */
                Errno::EINPROGRESS => {
                    nonblocking_polls += 1;
                    if nonblocking_polls >= MAX_NONBLOCKING_POLLS {
                        error!("Connect() not ready after retries when sending socket to: {path}",);
                        break Err(e);
                    }
                    warn!("Connect() not ready, will try again in {NONBLOCKING_POLL_INTERVAL:?}",);
                    thread::sleep(NONBLOCKING_POLL_INTERVAL);
                }
                _ => {
                    error!("Error sending socket to: {path}, error: {e:?}");
                    break Err(e);
                }
            },
        }
    };

    let result = match conn_result {
        Ok(_) => {
            let io_vec = [IoSlice::new(payload); 1];
            let scm = socket::ControlMessage::ScmRights(fds.as_slice());
            let cmsg = [scm; 1];
            loop {
                match socket::sendmsg(
                    send_fd,
                    &io_vec,
                    &cmsg,
                    socket::MsgFlags::empty(),
                    None::<&UnixAddr>,
                ) {
                    Ok(result) => break Ok(result),
                    Err(e) => match e {
                        /* handle nonblocking IO */
                        Errno::EAGAIN => {
                            nonblocking_polls += 1;
                            if nonblocking_polls >= MAX_NONBLOCKING_POLLS {
                                error!(
                                    "Sendmsg() not ready after retries when sending socket to: {}",
                                    path
                                );
                                break Err(e);
                            }
                            warn!(
                                "Sendmsg() not ready, will try again in {:?}",
                                NONBLOCKING_POLL_INTERVAL
                            );
                            thread::sleep(NONBLOCKING_POLL_INTERVAL);
                        }
                        _ => break Err(e),
                    },
                }
            }
        }
        Err(_) => conn_result,
    };

    nix::unistd::close(send_fd).unwrap();
    result
}

#[cfg(not(target_os = "linux"))]
pub fn send_fds_to<P>(_fds: Vec<RawFd>, _payload: &[u8], _path: &P) -> Result<usize, Error>
where
    P: ?Sized + NixPath + std::fmt::Display,
{
    Ok(0)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use log::{debug, error};

    fn init_log() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[test]
    fn test_add_get() {
        init_log();
        let mut fds = Fds::new();
        let key = "1.1.1.1:80".to_string();
        fds.add(key.clone(), 128);
        assert_eq!(128, *fds.get(&key).unwrap());
    }

    #[test]
    fn test_table_serde() {
        init_log();
        let mut fds = Fds::new();
        let key1 = "1.1.1.1:80".to_string();
        fds.add(key1.clone(), 128);
        let key2 = "1.1.1.1:443".to_string();
        fds.add(key2.clone(), 129);

        let (k, v) = fds.serialize();
        let mut fds2 = Fds::new();
        fds2.deserialize(k, v);

        assert_eq!(128, *fds2.get(&key1).unwrap());
        assert_eq!(129, *fds2.get(&key2).unwrap());
    }

    #[test]
    fn test_vec_string_serde() {
        init_log();
        let vec_str: Vec<String> = vec!["aaaa".to_string(), "bbb".to_string()];
        let mut ser_buf: [u8; 1024] = [0; 1024];
        let size = serialize_vec_string(&vec_str, &mut ser_buf);
        let de_vec_string = deserialize_vec_string(&ser_buf[..size]).unwrap();
        assert_eq!(de_vec_string.len(), 2);
        assert_eq!(de_vec_string[0], "aaaa");
        assert_eq!(de_vec_string[1], "bbb");
    }

    #[test]
    fn test_send_receive_fds() {
        init_log();
        let dumb_fd = socket::socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .unwrap();

        // receiver need to start in another thread since it is blocking
        let child = thread::spawn(move || {
            let mut buf: [u8; 32] = [0; 32];
            let (fds, bytes) = get_fds_from("/tmp/pingora_fds_receive.sock", &mut buf).unwrap();
            debug!("{:?}", fds);
            assert_eq!(1, fds.len());
            assert_eq!(32, bytes);
            assert_eq!(1, buf[0]);
            assert_eq!(1, buf[31]);
        });

        let fds = vec![dumb_fd];
        let buf: [u8; 128] = [1; 128];
        match send_fds_to(fds, &buf, "/tmp/pingora_fds_receive.sock") {
            Ok(sent) => {
                assert!(sent > 0);
            }
            Err(e) => {
                error!("{:?}", e);
                panic!()
            }
        }

        child.join().unwrap();
    }

    #[test]
    fn test_serde_via_socket() {
        init_log();
        let mut fds = Fds::new();
        let key1 = "1.1.1.1:80".to_string();
        let dumb_fd1 = socket::socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        fds.add(key1.clone(), dumb_fd1);
        let key2 = "1.1.1.1:443".to_string();
        let dumb_fd2 = socket::socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        fds.add(key2.clone(), dumb_fd2);

        let child = thread::spawn(move || {
            let mut fds2 = Fds::new();
            fds2.get_from_sock("/tmp/pingora_fds_receive2.sock")
                .unwrap();
            assert!(*fds2.get(&key1).unwrap() > 0);
            assert!(*fds2.get(&key2).unwrap() > 0);
        });

        fds.send_to_sock("/tmp/pingora_fds_receive2.sock").unwrap();
        child.join().unwrap();
    }
}
