// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Cancellable, deadline-bounded Bluetooth datagram I/O.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use librepods_core::protocol::packets;
use librepods_core::transport::{L2capTransport, TransportError};
use nix::libc;

use crate::error::{ErrorKind, PlatformError};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[repr(C)]
struct SockAddrL2 {
    l2_family: u16,
    l2_psm: u16,
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

fn parse_bdaddr(address: &str) -> Result<[u8; 6], TransportError> {
    let mut bytes = [0u8; 6];
    let mut components = address.split(':');
    for byte in bytes.iter_mut().rev() {
        let part = components.next().ok_or_else(|| {
            PlatformError::new("parse Bluetooth address", ErrorKind::InvalidInput)
        })?;
        if part.len() != 2 {
            return Err(
                PlatformError::new("parse Bluetooth address", ErrorKind::InvalidInput).into(),
            );
        }
        *byte = u8::from_str_radix(part, 16)
            .map_err(|_| PlatformError::new("parse Bluetooth address", ErrorKind::InvalidInput))?;
    }
    if components.next().is_some() {
        return Err(PlatformError::new("parse Bluetooth address", ErrorKind::InvalidInput).into());
    }
    Ok(bytes)
}

struct CancellationState {
    event: OwnedFd,
    cancelled: AtomicBool,
}

/// Cancellation wakes pending connect, receive and send waits immediately.
#[derive(Clone)]
pub struct Cancellation(Arc<CancellationState>);

impl Cancellation {
    pub fn new() -> Result<Self, TransportError> {
        // SAFETY: eventfd returns an owned descriptor or a negative error.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(
                PlatformError::io("create cancellation event", io::Error::last_os_error()).into(),
            );
        }
        Ok(Self(Arc::new(CancellationState {
            // SAFETY: fd was created here and has one owner.
            event: unsafe { OwnedFd::from_raw_fd(fd) },
            cancelled: AtomicBool::new(false),
        })))
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::AcqRel) {
            let wake = 1u64;
            // SAFETY: wake points to a live eventfd-sized value.
            let _ = unsafe {
                libc::write(
                    self.0.event.as_raw_fd(),
                    (&wake as *const u64).cast(),
                    size_of::<u64>(),
                )
            };
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
}

fn deadline(timeout: Duration) -> Result<Instant, TransportError> {
    Instant::now().checked_add(timeout).ok_or_else(|| {
        PlatformError::new("compute transport deadline", ErrorKind::InvalidInput).into()
    })
}

fn wait(
    fd: RawFd,
    events: i16,
    until: Instant,
    cancel: &Cancellation,
    operation: &'static str,
) -> Result<(), TransportError> {
    loop {
        if cancel.is_cancelled() {
            return Err(PlatformError::new(operation, ErrorKind::Cancelled).into());
        }
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PlatformError::new(operation, ErrorKind::Timeout).into());
        }
        let mut descriptors = [
            libc::pollfd {
                fd,
                events,
                revents: 0,
            },
            libc::pollfd {
                fd: cancel.0.event.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let millis = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        // SAFETY: descriptors is an initialized array of the supplied length.
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                millis,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(PlatformError::io(operation, error).into());
        }
        if result == 0 {
            continue;
        }
        if cancel.is_cancelled() || descriptors[1].revents != 0 {
            return Err(PlatformError::new(operation, ErrorKind::Cancelled).into());
        }
        // A final readable datagram may accompany HUP, and must be consumed first.
        if descriptors[0].revents & events != 0 {
            return Ok(());
        }
        if descriptors[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(PlatformError::new(operation, ErrorKind::Unavailable).into());
        }
    }
}

pub struct L2capLinux {
    fd: OwnedFd,
    cancellation: Cancellation,
    connected: AtomicBool,
}

impl L2capLinux {
    pub fn connect(address: &str) -> Result<Self, TransportError> {
        Self::connect_timeout(address, DEFAULT_TIMEOUT)
    }

    pub fn connect_timeout(address: &str, timeout: Duration) -> Result<Self, TransportError> {
        Self::connect_with_cancel(address, packets::AAP_PSM, timeout, Cancellation::new()?)
    }

    pub fn connect_psm(address: &str, psm: u16) -> Result<Self, TransportError> {
        Self::connect_with_cancel(address, psm, DEFAULT_TIMEOUT, Cancellation::new()?)
    }

    pub fn connect_psm_timeout(
        address: &str,
        psm: u16,
        timeout: Duration,
    ) -> Result<Self, TransportError> {
        Self::connect_with_cancel(address, psm, timeout, Cancellation::new()?)
    }

    pub fn connect_with_cancel(
        address: &str,
        psm: u16,
        timeout: Duration,
        cancellation: Cancellation,
    ) -> Result<Self, TransportError> {
        let until = deadline(timeout)?;
        let destination = SockAddrL2 {
            l2_family: libc::AF_BLUETOOTH as u16,
            l2_psm: psm.to_le(),
            l2_bdaddr: parse_bdaddr(address)?,
            l2_cid: 0,
            l2_bdaddr_type: 0,
        };
        if cancellation.is_cancelled() {
            return Err(PlatformError::new("connect L2CAP", ErrorKind::Cancelled).into());
        }
        // SAFETY: constants describe a nonblocking, close-on-exec Bluetooth socket.
        let raw_fd = unsafe {
            libc::socket(
                libc::AF_BLUETOOTH,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if raw_fd < 0 {
            return Err(
                PlatformError::io("create L2CAP socket", io::Error::last_os_error()).into(),
            );
        }
        // SAFETY: raw_fd was created here and is not otherwise owned.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        // SAFETY: destination is a correctly sized sockaddr_l2.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&destination as *const SockAddrL2).cast(),
                size_of::<SockAddrL2>() as libc::socklen_t,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EAGAIN)) {
                return Err(PlatformError::io("connect L2CAP", error).into());
            }
            wait(
                fd.as_raw_fd(),
                libc::POLLOUT | libc::POLLERR,
                until,
                &cancellation,
                "connect L2CAP",
            )?;
            let mut error = 0i32;
            let mut size = size_of::<i32>() as libc::socklen_t;
            // SAFETY: both output pointers refer to initialized values with valid sizes.
            let result = unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut error as *mut i32).cast(),
                    &mut size,
                )
            };
            if result != 0 {
                return Err(PlatformError::io(
                    "inspect L2CAP connection",
                    io::Error::last_os_error(),
                )
                .into());
            }
            if error != 0 {
                return Err(PlatformError::io(
                    "connect L2CAP",
                    io::Error::from_raw_os_error(error),
                )
                .into());
            }
        }
        Ok(Self {
            fd,
            cancellation,
            connected: AtomicBool::new(true),
        })
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    pub fn send_timeout(&self, packet: &[u8], timeout: Duration) -> Result<(), TransportError> {
        let until = deadline(timeout)?;
        loop {
            wait(
                self.fd.as_raw_fd(),
                libc::POLLOUT,
                until,
                &self.cancellation,
                "send L2CAP",
            )?;
            // SAFETY: packet is a live byte slice; MSG_NOSIGNAL prevents process termination on closed peers.
            let count = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if count >= 0 {
                return if count as usize == packet.len() {
                    Ok(())
                } else {
                    Err(
                        PlatformError::new("send partial L2CAP datagram", ErrorKind::Truncated)
                            .into(),
                    )
                };
            }
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(PlatformError::io("send L2CAP", error).into());
        }
    }

    pub fn recv_timeout(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        let until = deadline(timeout)?;
        loop {
            wait(
                self.fd.as_raw_fd(),
                libc::POLLIN,
                until,
                &self.cancellation,
                "receive L2CAP",
            )?;
            let mut vector = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            // SAFETY: msghdr supports all-zero initialization and the vector is valid until recvmsg returns.
            let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
            message.msg_iov = &mut vector;
            message.msg_iovlen = 1;
            // SAFETY: message points to a valid receive buffer and initialized msghdr.
            let count = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut message, 0) };
            if count > 0 {
                if message.msg_flags & libc::MSG_TRUNC != 0 {
                    return Err(PlatformError::new(
                        "receive truncated L2CAP datagram",
                        ErrorKind::Truncated,
                    )
                    .into());
                }
                return Ok(count as usize);
            }
            if count == 0 {
                self.connected.store(false, Ordering::Release);
                return Err(TransportError::ConnectionClosed);
            }
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(PlatformError::io("receive L2CAP", error).into());
        }
    }
}

impl L2capTransport for L2capLinux {
    fn send(&self, packet: &[u8]) -> Result<(), TransportError> {
        self.send_timeout(packet, DEFAULT_TIMEOUT)
    }
    fn recv(&self, buf: &mut [u8]) -> Result<usize, TransportError> {
        loop {
            match self.recv_timeout(buf, DEFAULT_TIMEOUT) {
                Err(error) if crate::error::kind(&error) == Some(ErrorKind::Timeout) => continue,
                result => return result,
            }
        }
    }
    fn recv_timeout(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        Self::recv_timeout(self, buf, timeout)
    }
    fn close(&self) -> Result<(), TransportError> {
        self.connected.store(false, Ordering::Release);
        self.cancellation.cancel();
        // SAFETY: shutdown changes socket state, but does not release/reuse the owned fd.
        let result = unsafe { libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOTCONN) {
                return Err(PlatformError::io("shutdown L2CAP", error).into());
            }
        }
        Ok(())
    }
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire) && !self.cancellation.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::kind;

    fn pair() -> (Arc<L2capLinux>, OwnedFd) {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors points to two integer outputs.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                    descriptors.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: socketpair created each distinct descriptor here.
        let (fd, peer) = unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        };
        (
            Arc::new(L2capLinux {
                fd,
                cancellation: Cancellation::new().expect("eventfd"),
                connected: AtomicBool::new(true),
            }),
            peer,
        )
    }

    #[test]
    fn address_parser_is_strict() {
        assert_eq!(
            parse_bdaddr("AA:BB:CC:DD:EE:FF").expect("valid"),
            [255, 238, 221, 204, 187, 170]
        );
        for invalid in [
            "A:B:C:D:E:F",
            "AA:BB:CC:DD:EE:FF:00",
            "AA:BB:CC:DD:EE",
            "GG:BB:CC:DD:EE:FF",
        ] {
            assert!(parse_bdaddr(invalid).is_err());
        }
    }

    #[test]
    fn receive_preserves_datagrams_and_rejects_truncation() {
        let (socket, peer) = pair();
        assert_eq!(
            unsafe { libc::send(peer.as_raw_fd(), [1u8, 2, 3].as_ptr().cast(), 3, 0) },
            3
        );
        let mut buf = [0u8; 2];
        assert_eq!(
            kind(
                &socket
                    .recv_timeout(&mut buf, Duration::from_secs(1))
                    .expect_err("oversized")
            ),
            Some(ErrorKind::Truncated)
        );
        assert_eq!(
            unsafe { libc::send(peer.as_raw_fd(), [7u8].as_ptr().cast(), 1, 0) },
            1
        );
        assert_eq!(
            socket
                .recv_timeout(&mut buf, Duration::from_secs(1))
                .expect("next datagram"),
            1
        );
        assert_eq!(buf[0], 7);
    }

    #[test]
    fn timeout_is_real_and_close_wakes_pending_read() {
        let (socket, _peer) = pair();
        assert_eq!(
            kind(
                &socket
                    .recv_timeout(&mut [0u8; 8], Duration::ZERO)
                    .expect_err("deadline")
            ),
            Some(ErrorKind::Timeout)
        );
        let reader = socket.clone();
        let thread =
            std::thread::spawn(move || reader.recv_timeout(&mut [0u8; 8], Duration::from_secs(60)));
        socket.close().expect("close");
        assert_eq!(
            kind(
                &thread
                    .join()
                    .expect("reader joined")
                    .expect_err("cancelled")
            ),
            Some(ErrorKind::Cancelled)
        );
        assert!(!socket.is_connected());
    }

    #[test]
    fn send_and_peer_close_are_reported_without_sigpipe() {
        let (socket, peer) = pair();
        socket
            .send_timeout(&[9], Duration::from_secs(1))
            .expect("send");
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::recv(peer.as_raw_fd(), (&mut byte as *mut u8).cast(), 1, 0) },
            1
        );
        assert_eq!(byte, 9);
        // shutdown propagates even if another parallel test briefly forked before CLOEXEC.
        assert_eq!(
            unsafe { libc::shutdown(peer.as_raw_fd(), libc::SHUT_RDWR) },
            0
        );
        drop(peer);
        assert!(socket.send_timeout(&[1], Duration::from_secs(1)).is_err());
    }
    #[test]
    fn blocked_send_deadline_and_cancellation_wake_without_dropping_descriptors() {
        let (socket, peer) = pair();
        let value: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    socket.fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&value as *const libc::c_int).cast(),
                    std::mem::size_of_val(&value) as libc::socklen_t,
                )
            },
            0
        );
        let payload = [7u8; 1024];
        loop {
            let sent = unsafe {
                libc::send(
                    socket.fd.as_raw_fd(),
                    payload.as_ptr().cast(),
                    payload.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if sent < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                break;
            }
        }
        let started = Instant::now();
        assert_eq!(
            kind(
                &socket
                    .send_timeout(&payload, Duration::from_millis(20))
                    .expect_err("full socket send deadline")
            ),
            Some(ErrorKind::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let pending = socket.clone();
        let writer =
            std::thread::spawn(move || pending.send_timeout(&payload, Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(10));
        socket.cancellation().cancel();
        assert_eq!(
            kind(
                &writer
                    .join()
                    .expect("writer thread")
                    .expect_err("cancelled send")
            ),
            Some(ErrorKind::Cancelled)
        );
        assert!(!socket.is_connected());
        drop(peer);
    }

    #[test]
    fn pre_cancelled_connect_and_trait_receive_are_safe_without_bluetooth_hardware() {
        let cancellation = Cancellation::new().expect("cancellation");
        cancellation.cancel();
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
        let result = L2capLinux::connect_with_cancel(
            "AA:BB:CC:DD:EE:FF",
            0x1001,
            Duration::from_secs(30),
            cancellation,
        );
        assert!(matches!(result, Err(ref error) if kind(error) == Some(ErrorKind::Cancelled)));
        assert!(L2capLinux::connect("invalid").is_err());
        assert!(L2capLinux::connect_psm("invalid", 0x1001).is_err());
        assert!(L2capLinux::connect_psm_timeout("invalid", 0x1001, Duration::MAX).is_err());
        let (socket, peer) = pair();
        L2capTransport::send(&*socket, &[4, 5]).expect("trait send");
        let mut bytes = [0u8; 8];
        assert_eq!(
            unsafe { libc::recv(peer.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0) },
            2
        );
        assert_eq!(
            unsafe { libc::send(peer.as_raw_fd(), [9u8].as_ptr().cast(), 1, 0) },
            1
        );
        assert_eq!(
            L2capTransport::recv(&*socket, &mut bytes).expect("trait receive"),
            1
        );
        assert_eq!(bytes[0], 9);
        drop(peer);
        assert!(matches!(
            L2capTransport::recv_timeout(&*socket, &mut bytes, Duration::from_secs(1)),
            Err(TransportError::ConnectionClosed)
        ));
        assert!(!socket.is_connected());
        socket.close().expect("close after peer EOF");
        socket.close().expect("idempotent close");
    }
}
