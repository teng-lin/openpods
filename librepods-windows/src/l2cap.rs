// LibrePods - AirPods liberated from Apple's ecosystem
// Copyright (C) 2025 LibrePods contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! L2CAP transport implementation using WinSock Bluetooth sockets (`AF_BTH`).
//!
//! Windows exposes classic Bluetooth L2CAP to user space through WinSock:
//! create a socket with `AF_BTH` + `SOCK_SEQPACKET` + `BTHPROTO_L2CAP`, then
//! connect with a [`SOCKADDR_BTH`] whose `port` field carries the PSM and whose
//! `btAddr` carries the 48-bit device address.
//!
//! Reference: Microsoft Winsock Bluetooth documentation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use librepods_core::protocol::packets;
use librepods_core::transport::{L2capTransport, TransportError};
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_L2CAP, SOCKADDR_BTH};
use windows_sys::Win32::Networking::WinSock::{
    closesocket as ws_close, connect as ws_connect, recv as ws_recv, send as ws_send,
    socket as ws_socket, WSAGetLastError, WSAStartup, INVALID_SOCKET, SOCKADDR, SOCKET,
    SOCK_SEQPACKET, WSADATA,
};

// ---------------------------------------------------------------------------
// Winsock initialisation
// ---------------------------------------------------------------------------

/// Result of the one-time `WSAStartup` call.
static WSA_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Initialise Winsock once per process (idempotent).
fn ensure_wsa() -> Result<(), TransportError> {
    let result = WSA_INIT.get_or_init(|| unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        // MAKEWORD(2, 2) — request Winsock 2.2.
        let rc = WSAStartup(0x0202, &mut data);
        if rc != 0 {
            Err(format!("WSAStartup failed with error {rc}"))
        } else {
            Ok(())
        }
    });
    result.clone().map_err(TransportError::new)
}

// ---------------------------------------------------------------------------
// Address parsing
// ---------------------------------------------------------------------------

/// Parse a Bluetooth MAC address string (e.g. `"AA:BB:CC:DD:EE:FF"`) into the
/// 48-bit `BTH_ADDR` representation expected by WinSock (big-endian: the first
/// octet is the most significant byte).
fn parse_bdaddr(addr: &str) -> Result<u64, TransportError> {
    let parts: Vec<&str> = addr.split(':').collect();
    if parts.len() != 6 {
        return Err(TransportError::new(format!(
            "invalid Bluetooth address: {addr}"
        )));
    }
    let mut value: u64 = 0;
    for part in parts {
        let byte = u8::from_str_radix(part, 16)
            .map_err(|e| TransportError::with_source(format!("bad hex in address: {part}"), e))?;
        value = (value << 8) | byte as u64;
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// L2CAP socket implementation
// ---------------------------------------------------------------------------

/// Windows L2CAP socket transport using WinSock `AF_BTH` sockets.
pub struct L2capWindows {
    sock: SOCKET,
    connected: AtomicBool,
}

impl L2capWindows {
    /// Connect to an AirPods device at `address` (e.g. `"AA:BB:CC:DD:EE:FF"`)
    /// on PSM [`packets::AAP_PSM`] (0x1001).
    pub fn connect(address: &str) -> Result<Self, TransportError> {
        Self::connect_psm(address, packets::AAP_PSM)
    }

    /// Connect to a device on a specific PSM.
    pub fn connect_psm(address: &str, psm: u16) -> Result<Self, TransportError> {
        ensure_wsa()?;
        let bdaddr = parse_bdaddr(address)?;

        let sock = unsafe { ws_socket(AF_BTH as i32, SOCK_SEQPACKET, BTHPROTO_L2CAP as i32) };
        if sock == INVALID_SOCKET {
            let err = unsafe { WSAGetLastError() };
            return Err(TransportError::with_source(
                format!(
                    "failed to create L2CAP socket (AF_BTH, SOCK_SEQPACKET, BTHPROTO_L2CAP): \
                 Winsock error {err}. Is the Bluetooth adapter enabled?"
                ),
                std::io::Error::from_raw_os_error(err),
            ));
        }

        let sockaddr = SOCKADDR_BTH {
            addressFamily: AF_BTH,
            btAddr: bdaddr,
            serviceClassId: GUID {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            },
            port: psm as u32,
        };

        let ret = unsafe {
            ws_connect(
                sock,
                &sockaddr as *const SOCKADDR_BTH as *const SOCKADDR,
                std::mem::size_of::<SOCKADDR_BTH>() as i32,
            )
        };
        if ret != 0 {
            let err = unsafe { WSAGetLastError() };
            unsafe { ws_close(sock) };
            return Err(TransportError::with_source(
                format!(
                    "L2CAP connect to {address} PSM 0x{psm:04X} failed (Winsock error {err}). \
                     Ensure the AirPods are paired, connected and in range."
                ),
                std::io::Error::from_raw_os_error(err),
            ));
        }

        log::info!("L2CAP connected to {address} PSM 0x{psm:04X}");

        Ok(Self {
            sock,
            connected: AtomicBool::new(true),
        })
    }
}

impl L2capTransport for L2capWindows {
    fn send(&self, data: &[u8]) -> Result<(), TransportError> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err(TransportError::NotConnected);
        }
        if data.len() > i32::MAX as usize {
            return Err(TransportError::new("packet too large to send"));
        }
        let n = unsafe { ws_send(self.sock, data.as_ptr(), data.len() as i32, 0) };
        if n < 0 {
            let err = unsafe { WSAGetLastError() };
            return Err(TransportError::with_source(
                format!("L2CAP send failed (Winsock error {err})"),
                std::io::Error::from_raw_os_error(err),
            ));
        }
        if n as usize != data.len() {
            return Err(TransportError::new(format!(
                "short write: {n}/{} bytes",
                data.len()
            )));
        }
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if !self.connected.load(Ordering::Relaxed) {
            return Err(TransportError::NotConnected);
        }
        if buf.len() > i32::MAX as usize {
            return Err(TransportError::new("receive buffer too large"));
        }
        let n = unsafe { ws_recv(self.sock, buf.as_mut_ptr(), buf.len() as i32, 0) };
        if n < 0 {
            let err = unsafe { WSAGetLastError() };
            return Err(TransportError::with_source(
                format!("L2CAP recv failed (Winsock error {err})"),
                std::io::Error::from_raw_os_error(err),
            ));
        }
        if n == 0 {
            self.connected.store(false, Ordering::Relaxed);
            return Err(TransportError::ConnectionClosed);
        }
        Ok(n as usize)
    }

    fn close(&self) -> Result<(), TransportError> {
        if self.connected.swap(false, Ordering::Relaxed) {
            unsafe { ws_close(self.sock) };
        }
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

impl Drop for L2capWindows {
    fn drop(&mut self) {
        if self.connected.swap(false, Ordering::Relaxed) {
            unsafe { ws_close(self.sock) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bdaddr_be() {
        assert_eq!(parse_bdaddr("AA:BB:CC:DD:EE:FF").unwrap(), 0xAABBCCDDEEFF);
        assert_eq!(parse_bdaddr("00:00:00:00:00:00").unwrap(), 0);
    }

    #[test]
    fn parse_bdaddr_rejects_bad_input() {
        assert!(parse_bdaddr("AA:BB:CC").is_err());
        assert!(parse_bdaddr("ZZ:BB:CC:DD:EE:FF").is_err());
    }
}
