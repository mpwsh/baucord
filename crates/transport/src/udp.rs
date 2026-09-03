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

/// A bound, nonblocking UDP socket with the `quinn-udp` extras.
pub struct UdpIo {
    socket: UdpSocket,
    state: UdpSocketState,
    bufs: Vec<Vec<u8>>,
    metas: Vec<RecvMeta>,
    /// Datagrams dropped because the kernel would not take them.
    send_drops: u64,
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
        })
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Everything queued on the socket comes out of one `recv_batch`
    /// call, however the platform's `recv` batches — the bug this
    /// guards against left all but the first datagram in the kernel
    /// queue on macOS and Windows.
    #[test]
    fn recv_batch_drains_the_whole_socket_queue() {
        let mut rx = UdpIo::bind("127.0.0.1:0".parse().unwrap(), 1 << 20).expect("bind rx");
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind tx");
        let to = rx.local_addr().expect("addr");
        const N: usize = 300;
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
