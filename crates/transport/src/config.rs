//! Per-side transport budgets (spec §4.5).
//!
//! quinn-proto's own defaults are tuned for bulk transfer; left alone
//! they would grow the relay from a few MB idle to tens of MB and add
//! latency-shaped buffers. Everything here is a **[default]** to confirm
//! against measured footprint at M5; the shape is what matters: every
//! real-time queue has a byte bound *and* (in the driver) a time bound.

use std::{sync::Arc, time::Duration};

use quinn_proto::{AckFrequencyConfig, IdleTimeout, MtuDiscoveryConfig, TransportConfig, VarInt};

/// One side's transport budget.
#[derive(Debug, Clone)]
pub struct Budget {
    /// No traffic for this long ⇒ the connection is dead. Path changes
    /// do not count as idle.
    pub max_idle_timeout: Duration,
    /// Send a PING when idle for this long (`None`: the peer keeps the
    /// connection alive).
    pub keep_alive_interval: Option<Duration>,
    /// Peer-advertised ack delay bound.
    pub max_ack_delay: Duration,
    /// Buffered inbound DATAGRAM frames not yet read by the app.
    pub datagram_receive_buffer: usize,
    /// Outbound DATAGRAM frames waiting for a packet.
    pub datagram_send_buffer: usize,
    /// Flow-control credit we grant the peer per stream.
    pub stream_receive_window: u64,
    /// Flow-control credit we grant the peer for the whole connection.
    pub receive_window: u64,
    /// Unacknowledged stream data we hold, all streams together. This
    /// is the transport-level **depth bound** for video (§6.2): once it
    /// is full, writes block, bytes wait in the driver's own queues with
    /// timestamps, and the 500 ms deadline can act on them. 512 KiB is
    /// 0.5 s at the top video rung and still 40 Mbit/s at 100 ms RTT.
    pub send_window: u64,
    /// Unidirectional streams the peer may have open at once.
    pub max_concurrent_uni_streams: u32,
    /// Bidirectional streams the peer may have open at once.
    pub max_concurrent_bidi_streams: u32,
    pub initial_rtt: Duration,
    /// Bytes per second at which Media-lane uploads drain onto the
    /// wire, so bulk never fills the bottleneck queue voice shares.
    pub media_pace_bytes_per_sec: usize,
    /// Kernel UDP socket buffer target, send and receive.
    pub socket_buffer_bytes: usize,
    /// Mark outgoing packets as voice-class (DSCP EF / qWAVE voice
    /// flow) so home routers and gaming `QoS` engines prioritize them
    /// instead of dropping unknown UDP under contention. Client side;
    /// the relay's packets are marked by whoever runs its network.
    pub mark_voice: bool,
}

/// Datagram MTU everything assumes until discovery raises it: voice fits
/// with room to spare, video chunks were sized for it.
pub const INITIAL_MTU: u16 = 1200;

impl Budget {
    /// The desktop / mobile client: small buffers, keeps the connection
    /// alive itself.
    #[must_use]
    pub fn client() -> Self {
        Self {
            // 30 s (spec: 15 s): a Wi-Fi roam or ISP hiccup of 15–25 s
            // now survives instead of ending the session; the roster
            // shows a dead peer for up to 30 s in exchange. Keep-alives
            // every 2 s so a lossy link still lands some in the window.
            max_idle_timeout: Duration::from_secs(30),
            keep_alive_interval: Some(Duration::from_secs(2)),
            max_ack_delay: Duration::from_millis(25),
            datagram_receive_buffer: 64 * 1024,
            datagram_send_buffer: 32 * 1024,
            stream_receive_window: 512 * 1024,
            receive_window: 2 * 1024 * 1024,
            send_window: 512 * 1024,
            max_concurrent_uni_streams: 32,
            max_concurrent_bidi_streams: 4,
            initial_rtt: Duration::from_millis(100),
            media_pace_bytes_per_sec: protocol::MEDIA_UPLOAD_PACE,
            socket_buffer_bytes: 8 * 1024 * 1024,
            mark_voice: true,
        }
    }

    /// The relay: RAM buys headroom, never depth — video and datagram
    /// queues are additionally time-bounded in the driver.
    #[must_use]
    pub fn server() -> Self {
        Self {
            // Matches the client; QUIC uses the smaller of the two.
            max_idle_timeout: Duration::from_secs(30),
            keep_alive_interval: None,
            max_ack_delay: Duration::from_millis(25),
            datagram_receive_buffer: 256 * 1024,
            datagram_send_buffer: 256 * 1024,
            stream_receive_window: 2 * 1024 * 1024,
            receive_window: 16 * 1024 * 1024,
            // Per viewer connection; see the field doc. The spec's 32 MiB
            // would be 30 s of depth toward a behind viewer.
            send_window: 512 * 1024,
            max_concurrent_uni_streams: 64,
            max_concurrent_bidi_streams: 4,
            initial_rtt: Duration::from_millis(100),
            media_pace_bytes_per_sec: 1024 * 1024,
            socket_buffer_bytes: 64 * 1024 * 1024,
            mark_voice: false,
        }
    }

    /// The quinn-proto configuration for this budget.
    ///
    /// # Panics
    /// If a window exceeds the QUIC varint range (2^62), which no budget
    /// here approaches.
    #[must_use]
    pub fn transport_config(&self) -> Arc<TransportConfig> {
        let mut cfg = TransportConfig::default();
        cfg.max_idle_timeout(Some(
            IdleTimeout::try_from(self.max_idle_timeout).expect("idle timeout fits a varint"),
        ));
        cfg.keep_alive_interval(self.keep_alive_interval);
        let mut ack = AckFrequencyConfig::default();
        ack.max_ack_delay(Some(self.max_ack_delay));
        cfg.ack_frequency_config(Some(ack));
        cfg.datagram_receive_buffer_size(Some(self.datagram_receive_buffer));
        cfg.datagram_send_buffer_size(self.datagram_send_buffer);
        cfg.stream_receive_window(varint(self.stream_receive_window));
        cfg.receive_window(varint(self.receive_window));
        cfg.send_window(self.send_window);
        cfg.max_concurrent_uni_streams(VarInt::from_u32(self.max_concurrent_uni_streams));
        cfg.max_concurrent_bidi_streams(VarInt::from_u32(self.max_concurrent_bidi_streams));
        cfg.initial_rtt(self.initial_rtt);
        cfg.initial_mtu(INITIAL_MTU);
        cfg.min_mtu(INITIAL_MTU);
        cfg.mtu_discovery_config(Some(MtuDiscoveryConfig::default()));
        // Decision 7: default controller (Cubic) with pacing; ladders
        // stay in charge. Fairness off: priorities decide who sends.
        cfg.send_fairness(false);
        Arc::new(cfg)
    }
}

fn varint(v: u64) -> VarInt {
    VarInt::from_u64(v).expect("window fits a varint")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_budgets_build_a_transport_config() {
        let _ = Budget::client().transport_config();
        let _ = Budget::server().transport_config();
    }

    #[test]
    fn client_keeps_alive_and_server_does_not() {
        assert!(Budget::client().keep_alive_interval.is_some());
        assert!(Budget::server().keep_alive_interval.is_none());
    }
}
