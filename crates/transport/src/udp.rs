//! The real socket. A thin, synchronous wrapper over `quinn-udp` so the
//! existing threads can pump a [`Driver`](crate::Driver) without a
//! runtime: receive everything that is ready, hand it to the driver,
//! send whatever the driver produced, sleep until its next timer.
//!
//! GSO/GRO, ECN and socket buffer sizing come from `quinn-udp`; failures
//! to send (`WouldBlock`, `ENOBUFS`) are treated as a dropped datagram —
//! QUIC recovers, and blocking the media thread would be worse.

use std::{
    io::{self, IoSliceMut},
    net::{SocketAddr, UdpSocket},
};

use bytes::BytesMut;
use quinn_udp::{RecvMeta, UdpSockRef, UdpSocketState};

use crate::driver::{InboundDatagram, Transmit};

/// Buffers offered per `recv` call. Linux `recvmmsg` fills up to all of
/// them; macOS and Windows return **one** datagram per call regardless.
const RECV_BATCH: usize = 16;
/// One receive buffer: large enough for a full GRO batch.
const RECV_BUF_BYTES: usize = 64 * 1024;
/// Datagrams one [`UdpIo::recv_batch`] call will deliver before handing
/// control back. A flood past this is reported as `drained: false` so
/// the loop comes straight back instead of sleeping.
const RECV_CALL_BUDGET: usize = 4096;

/// What one [`UdpIo::recv_batch`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvOutcome {
    /// Datagrams handed to the callback.
    pub delivered: usize,
    /// The socket queue was empty when we stopped. When false, more is
    /// waiting and the caller should not sleep before the next call.
    pub drained: bool,
}

/// DSCP "Expedited Forwarding" (46): the class routers and `QoS` engines
/// file interactive voice under. As a `TOS` byte it is `46 << 2`.
#[cfg(not(windows))]
const DSCP_EF: u32 = 46;

/// A bound, nonblocking UDP socket with the `quinn-udp` extras.
pub struct UdpIo {
    socket: UdpSocket,
    state: UdpSocketState,
    bufs: Vec<Vec<u8>>,
    metas: Vec<RecvMeta>,
    /// Datagrams dropped because the kernel would not take them.
    send_drops: u64,
    #[cfg(windows)]
    qos: Option<windows_qos::VoiceFlow>,
}

impl UdpIo {
    /// Bind and configure. `buffer_bytes` is the requested kernel
    /// send/receive buffer size (best effort).
    ///
    /// # Errors
    /// Bind failure or unsupported socket configuration.
    pub fn bind(addr: SocketAddr, buffer_bytes: usize) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        let state = UdpSocketState::new(UdpSockRef::from(&socket))?;
        if let Err(e) = state.set_send_buffer_size(UdpSockRef::from(&socket), buffer_bytes) {
            log::debug!("could not set UDP send buffer to {buffer_bytes}: {e}");
        }
        if let Err(e) = state.set_recv_buffer_size(UdpSockRef::from(&socket), buffer_bytes) {
            log::debug!("could not set UDP recv buffer to {buffer_bytes}: {e}");
        }
        log::info!(
            "UDP socket on {} (gso segments {}, gro segments {})",
            socket.local_addr()?,
            state.max_gso_segments(),
            state.gro_segments()
        );
        Ok(Self {
            socket,
            state,
            bufs: (0..RECV_BATCH).map(|_| vec![0u8; RECV_BUF_BYTES]).collect(),
            metas: vec![RecvMeta::default(); RECV_BATCH],
            send_drops: 0,
            #[cfg(windows)]
            qos: None,
        })
    }

    /// Mark everything this socket sends to `peer` as voice-class
    /// traffic (DSCP EF), so routers with `QoS` and "gaming" traffic
    /// engines file it with the interactive flows instead of dropping
    /// it as unknown UDP when the link is contended. Best effort: a
    /// stack that refuses it logs and carries on unmarked.
    ///
    /// On Unix this is the socket's TOS/traffic-class byte; Windows
    /// ignores those and needs a qWAVE flow (the API behind Discord's
    /// "High Packet Priority"), which is per destination — hence the
    /// `peer` argument, which the Unix path does not need.
    pub fn mark_voice(&mut self, peer: SocketAddr) {
        #[cfg(not(windows))]
        {
            // `set_tclass_v6` needs socket2's `all` feature (enabled in
            // Cargo.toml); `set_tos_v4` is always there.
            let sock = socket2::SockRef::from(&self.socket);
            let result = if peer.is_ipv6() {
                sock.set_tclass_v6(DSCP_EF << 2)
            } else {
                sock.set_tos_v4(DSCP_EF << 2)
            };
            match result {
                Ok(()) => log::info!("UDP socket marked DSCP EF (voice) toward {peer}"),
                Err(e) => log::info!("could not mark the UDP socket DSCP EF: {e}"),
            }
        }
        #[cfg(windows)]
        {
            match windows_qos::VoiceFlow::add(&self.socket, peer) {
                Ok(flow) => {
                    log::info!("UDP socket added to a qWAVE voice flow toward {peer}");
                    self.qos = Some(flow);
                }
                Err(e) => log::info!("qWAVE voice flow unavailable ({e}); sending unmarked"),
            }
        }
    }

    /// # Errors
    /// The socket has no local address (cannot happen once bound).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Segments one transmit may carry with GSO (1 without).
    #[must_use]
    pub fn max_gso_segments(&self) -> usize {
        self.state.max_gso_segments()
    }

    /// Datagrams the kernel refused to send so far.
    #[must_use]
    pub fn send_drops(&self) -> u64 {
        self.send_drops
    }

    /// Receive everything that is ready, calling `f` per datagram
    /// (GRO batches are split into their segments).
    ///
    /// Keeps calling `recv` until the socket reports `WouldBlock` — the
    /// count a single `recv` returns says nothing about what is still
    /// queued (macOS and Windows hand back one datagram per call), and
    /// leaving packets in the kernel queue between ticks is exactly the
    /// bufferbloat a relay must not add: at 5 Mbit/s of video that is
    /// ~430 packets/s, and a loop that took one per tick built up a
    /// second of queue on a LAN.
    ///
    /// # Errors
    /// A socket error other than `WouldBlock` / `Interrupted`.
    pub fn recv_batch(&mut self, mut f: impl FnMut(InboundDatagram)) -> io::Result<RecvOutcome> {
        let mut delivered = 0;
        loop {
            if delivered >= RECV_CALL_BUDGET {
                return Ok(RecvOutcome {
                    delivered,
                    drained: false,
                });
            }
            let mut slices: Vec<IoSliceMut<'_>> = self
                .bufs
                .iter_mut()
                .map(|b| IoSliceMut::new(b.as_mut_slice()))
                .collect();
            let n =
                match self
                    .state
                    .recv(UdpSockRef::from(&self.socket), &mut slices, &mut self.metas)
                {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return Ok(RecvOutcome {
                            delivered,
                            drained: true,
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
            drop(slices);
            for (buf, meta) in self.bufs.iter().zip(&self.metas).take(n) {
                if meta.len == 0 {
                    continue;
                }
                let data = &buf[..meta.len];
                // A GRO batch arrives as one buffer with `stride`-sized
                // segments; quinn-proto wants one datagram per call.
                for segment in data.chunks(meta.stride.max(1)) {
                    f(InboundDatagram {
                        remote: meta.addr,
                        local_ip: meta.dst_ip,
                        ecn: meta.ecn.map(|e| match e {
                            quinn_udp::EcnCodepoint::Ect0 => quinn_proto::EcnCodepoint::Ect0,
                            quinn_udp::EcnCodepoint::Ect1 => quinn_proto::EcnCodepoint::Ect1,
                            quinn_udp::EcnCodepoint::Ce => quinn_proto::EcnCodepoint::Ce,
                        }),
                        data: BytesMut::from(segment),
                    });
                    delivered += 1;
                }
            }
            if n == 0 {
                // Defensive: a zero-length read must not spin.
                return Ok(RecvOutcome {
                    delivered,
                    drained: true,
                });
            }
        }
    }

    /// Send one transmit (a GSO batch if `segment_size` is set). A
    /// kernel refusal is counted and swallowed.
    pub fn send(&mut self, t: &Transmit) {
        let transmit = quinn_udp::Transmit {
            destination: t.destination,
            ecn: t.ecn.map(|e| match e {
                quinn_proto::EcnCodepoint::Ect0 => quinn_udp::EcnCodepoint::Ect0,
                quinn_proto::EcnCodepoint::Ect1 => quinn_udp::EcnCodepoint::Ect1,
                quinn_proto::EcnCodepoint::Ce => quinn_udp::EcnCodepoint::Ce,
            }),
            contents: &t.contents,
            segment_size: t.segment_size,
            src_ip: t.src_ip,
        };
        if let Err(e) = self.state.send(UdpSockRef::from(&self.socket), &transmit) {
            self.send_drops += 1;
            if self.send_drops.is_power_of_two() {
                log::warn!(
                    "UDP send to {} failed ({e}); {} drops so far",
                    t.destination,
                    self.send_drops
                );
            }
        }
    }
}

#[cfg(windows)]
mod windows_qos {
    //! qWAVE: `QOSCreateHandle` once, `QOSAddSocketToFlow` with
    //! `QOSTrafficTypeVoice` for the (socket, destination) pair. Without
    //! administrator rights Windows picks the DSCP itself (voice ⇒ 56,
    //! CS7); that is still a top-priority class for every `QoS` engine.

    use std::{
        net::{SocketAddr, UdpSocket},
        os::windows::io::AsRawSocket,
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        NetworkManagement::QoS::{
            QOSAddSocketToFlow, QOSCreateHandle, QOSRemoveSocketFromFlow, QOSTrafficTypeVoice,
            QOS_NON_ADAPTIVE_FLOW, QOS_VERSION,
        },
        Networking::WinSock::{
            AF_INET, AF_INET6, IN6_ADDR, IN_ADDR, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
        },
    };

    pub struct VoiceFlow {
        handle: HANDLE,
        flow_id: u32,
    }

    impl VoiceFlow {
        pub fn add(socket: &UdpSocket, peer: SocketAddr) -> Result<Self, String> {
            let version = QOS_VERSION {
                MajorVersion: 1,
                MinorVersion: 0,
            };
            let mut handle: HANDLE = 0;
            // SAFETY: plain FFI with valid out-pointers; qwave.dll is part
            // of Windows Vista and later.
            let ok = unsafe { QOSCreateHandle(&raw const version, &raw mut handle) };
            if ok == 0 {
                return Err(format!(
                    "QOSCreateHandle: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut flow_id: u32 = 0;
            let ok = match peer {
                SocketAddr::V4(a) => {
                    let sa = SOCKADDR_IN {
                        sin_family: AF_INET,
                        sin_port: a.port().to_be(),
                        sin_addr: IN_ADDR {
                            S_un: windows_sys::Win32::Networking::WinSock::IN_ADDR_0 {
                                S_addr: u32::from_ne_bytes(a.ip().octets()),
                            },
                        },
                        sin_zero: [0; 8],
                    };
                    // SAFETY: `sa` outlives the call; the cast is the
                    // documented sockaddr punning.
                    unsafe {
                        QOSAddSocketToFlow(
                            handle,
                            socket.as_raw_socket() as usize,
                            std::ptr::addr_of!(sa).cast::<SOCKADDR>(),
                            QOSTrafficTypeVoice,
                            QOS_NON_ADAPTIVE_FLOW,
                            &raw mut flow_id,
                        )
                    }
                }
                SocketAddr::V6(a) => {
                    let sa = SOCKADDR_IN6 {
                        sin6_family: AF_INET6,
                        sin6_port: a.port().to_be(),
                        sin6_flowinfo: a.flowinfo(),
                        sin6_addr: IN6_ADDR {
                            u: windows_sys::Win32::Networking::WinSock::IN6_ADDR_0 {
                                Byte: a.ip().octets(),
                            },
                        },
                        Anonymous: windows_sys::Win32::Networking::WinSock::SOCKADDR_IN6_0 {
                            sin6_scope_id: a.scope_id(),
                        },
                    };
                    // SAFETY: as above.
                    unsafe {
                        QOSAddSocketToFlow(
                            handle,
                            socket.as_raw_socket() as usize,
                            std::ptr::addr_of!(sa).cast::<SOCKADDR>(),
                            QOSTrafficTypeVoice,
                            QOS_NON_ADAPTIVE_FLOW,
                            &raw mut flow_id,
                        )
                    }
                }
            };
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                // SAFETY: handle came from QOSCreateHandle.
                unsafe { CloseHandle(handle) };
                return Err(format!("QOSAddSocketToFlow: {err}"));
            }
            Ok(Self { handle, flow_id })
        }
    }

    impl Drop for VoiceFlow {
        fn drop(&mut self) {
            // SAFETY: both values came from the qWAVE calls above; a
            // socket already closed just makes the removal a no-op.
            unsafe {
                QOSRemoveSocketFromFlow(self.handle, 0, self.flow_id, 0);
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Everything queued on the socket comes out of one `recv_batch`
    /// call, however the platform's `recv` batches — the bug this
    /// guards against left all but the first datagram in the kernel
    /// queue on macOS and Windows.
    const N: usize = 300;

    #[test]
    fn recv_batch_drains_the_whole_socket_queue() {
        let mut rx = UdpIo::bind("127.0.0.1:0".parse().unwrap(), 1 << 20).expect("bind rx");
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind tx");
        let to = rx.local_addr().expect("addr");
        for i in 0..N {
            tx.send_to(&[u8::try_from(i % 256).unwrap(); 100], to)
                .expect("send");
        }
        // Let the loopback deliver.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        while got.len() < N && Instant::now() < deadline {
            let outcome = rx.recv_batch(|d| got.push(d.data.len())).expect("recv");
            if outcome.delivered == 0 {
                std::thread::sleep(Duration::from_millis(5));
            } else {
                // A single call must not stop while packets remain.
                assert!(outcome.drained || outcome.delivered >= RECV_CALL_BUDGET);
            }
        }
        assert_eq!(got.len(), N);
        assert!(got.iter().all(|len| *len == 100));
    }
}

#[cfg(all(test, not(windows)))]
mod dscp_tests {
    use super::*;

    /// The voice mark is a real socket option: after `mark_voice` the
    /// kernel reports DSCP EF on the socket.
    #[test]
    fn mark_voice_sets_dscp_ef_on_the_socket() {
        let mut io = UdpIo::bind("127.0.0.1:0".parse().unwrap(), 1 << 20).expect("bind");
        io.mark_voice("127.0.0.1:5000".parse().unwrap());
        let tos = socket2::SockRef::from(&io.socket)
            .tos_v4()
            .expect("read tos");
        assert_eq!(tos, DSCP_EF << 2, "expected EF (46 << 2 = 184), got {tos}");
    }
}
