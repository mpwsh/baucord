//! The simulation harness (spec §4.7): several [`Driver`]s on an
//! in-memory network with a fake clock and per-link delay, jitter,
//! loss, bandwidth and queue bound. Deterministic from a seed; a
//! 60-second scenario runs in well under a second.
//!
//! Every acceptance scenario in §10 that does not need a real NIC runs
//! here, in-process, in `cargo test`.

use std::{
    any::Any,
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    net::SocketAddr,
    time::{Duration, Instant},
};

use bytes::BytesMut;
use rand::{RngExt, SeedableRng};
use rand_pcg::Pcg64Mcg;

use crate::driver::{Driver, DriverHost, InboundDatagram};

/// One direction of a link.
#[derive(Debug, Clone)]
pub struct LinkSpec {
    /// One-way propagation delay.
    pub delay: Duration,
    /// Uniform random extra delay in `0..=jitter` (reorders packets).
    pub jitter: Duration,
    /// Independent drop probability per datagram.
    pub loss: f64,
    /// Bottleneck rate, bytes per second (`None`: unlimited).
    pub bandwidth_bytes_per_sec: Option<u64>,
    /// Router queue in front of the bottleneck; datagrams that would
    /// wait longer than `queue_bytes / bandwidth` are tail-dropped.
    pub queue_bytes: u64,
}

impl LinkSpec {
    /// A clean LAN-ish link.
    #[must_use]
    pub fn clean(delay: Duration) -> Self {
        Self {
            delay,
            jitter: Duration::ZERO,
            loss: 0.0,
            bandwidth_bytes_per_sec: None,
            queue_bytes: u64::MAX,
        }
    }

    #[must_use]
    pub fn with_loss(mut self, loss: f64) -> Self {
        self.loss = loss;
        self
    }

    #[must_use]
    pub fn with_jitter(mut self, jitter: Duration) -> Self {
        self.jitter = jitter;
        self
    }

    /// `bits_per_sec` bottleneck with a `queue_ms`-deep router queue.
    #[must_use]
    pub fn with_bandwidth(mut self, bits_per_sec: u64, queue_ms: u64) -> Self {
        let bytes = bits_per_sec / 8;
        self.bandwidth_bytes_per_sec = Some(bytes);
        self.queue_bytes = bytes * queue_ms / 1000;
        self
    }
}

struct Link {
    spec: LinkSpec,
    /// When the bottleneck finishes the last accepted datagram.
    next_free: Instant,
}

/// Handle to a node in the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(usize);

struct Node {
    addr: SocketAddr,
    host: Box<dyn DriverHost>,
}

struct InFlight {
    arrive_at: Instant,
    seq: u64,
    to: usize,
    from_addr: SocketAddr,
    data: Vec<u8>,
}

impl PartialEq for InFlight {
    fn eq(&self, o: &Self) -> bool {
        (self.arrive_at, self.seq) == (o.arrive_at, o.seq)
    }
}
impl Eq for InFlight {}
impl PartialOrd for InFlight {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for InFlight {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.arrive_at, self.seq).cmp(&(o.arrive_at, o.seq))
    }
}

/// The network.
pub struct SimNet {
    now: Instant,
    rng: Pcg64Mcg,
    nodes: Vec<Node>,
    links: HashMap<(usize, usize), Link>,
    in_flight: BinaryHeap<Reverse<InFlight>>,
    seq: u64,
    /// Datagrams dropped by loss, queue overflow or no route.
    pub dropped: u64,
    /// Datagrams delivered.
    pub delivered: u64,
}

impl SimNet {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            now: Instant::now(),
            rng: Pcg64Mcg::seed_from_u64(seed),
            nodes: Vec::new(),
            links: HashMap::new(),
            in_flight: BinaryHeap::new(),
            seq: 0,
            dropped: 0,
            delivered: 0,
        }
    }

    /// The fake clock.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.now
    }

    pub fn add_node(&mut self, host: impl DriverHost, addr: SocketAddr) -> NodeId {
        self.nodes.push(Node {
            addr,
            host: Box::new(host),
        });
        NodeId(self.nodes.len() - 1)
    }

    #[must_use]
    pub fn addr(&self, node: NodeId) -> SocketAddr {
        self.nodes[node.0].addr
    }

    /// Move a node to a new address (the client behind a NAT rebinding,
    /// or switching networks). Existing in-flight datagrams keep their
    /// old source.
    pub fn set_addr(&mut self, node: NodeId, addr: SocketAddr) {
        self.nodes[node.0].addr = addr;
    }

    pub fn driver(&mut self, node: NodeId) -> &mut Driver {
        self.nodes[node.0].host.driver()
    }

    /// The application hosting `node`'s driver, if it is a `T`.
    pub fn host_mut<T: DriverHost>(&mut self, node: NodeId) -> Option<&mut T> {
        let host: &mut dyn Any = self.nodes[node.0].host.as_mut();
        host.downcast_mut::<T>()
    }

    /// Both directions.
    pub fn set_link(&mut self, a: NodeId, b: NodeId, spec: LinkSpec) {
        self.set_link_one_way(a, b, spec.clone());
        self.set_link_one_way(b, a, spec);
    }

    pub fn set_link_one_way(&mut self, from: NodeId, to: NodeId, spec: LinkSpec) {
        self.links.insert(
            (from.0, to.0),
            Link {
                spec,
                next_free: self.now,
            },
        );
    }

    /// Advance the clock by `dt`, moving every datagram and firing every
    /// timer due in that window. Call with small steps (1–5 ms) for
    /// faithful timing.
    pub fn step(&mut self, dt: Duration) {
        // 1. Timers and per-tick work.
        for node in &mut self.nodes {
            node.host.driver().handle_timeouts(self.now);
        }
        // 2. Collect transmits and put them on the links.
        for i in 0..self.nodes.len() {
            let from_addr = self.nodes[i].addr;
            while let Some(t) = self.nodes[i].host.driver().poll_transmit() {
                let segment = t.segment_size.unwrap_or(t.contents.len()).max(1);
                for seg in t.contents.chunks(segment) {
                    self.route(i, from_addr, t.destination, seg);
                }
            }
        }
        // 3. Deliver everything due.
        self.now += dt;
        while let Some(Reverse(top)) = self.in_flight.peek() {
            if top.arrive_at > self.now {
                break;
            }
            let Some(Reverse(pkt)) = self.in_flight.pop() else {
                break;
            };
            let node = &mut self.nodes[pkt.to];
            let local_ip = node.addr.ip();
            node.host.driver().handle_datagram(
                pkt.arrive_at,
                InboundDatagram {
                    remote: pkt.from_addr,
                    local_ip: Some(local_ip),
                    ecn: None,
                    data: BytesMut::from(pkt.data.as_slice()),
                },
            );
            self.delivered += 1;
        }
    }

    /// Run for `total` in `tick` steps.
    pub fn run_for(&mut self, total: Duration, tick: Duration) {
        let end = self.now + total;
        while self.now < end {
            self.step(tick);
        }
    }

    /// Step until `done(self)` or `timeout`. Returns whether `done` hit.
    pub fn run_until(
        &mut self,
        timeout: Duration,
        tick: Duration,
        mut done: impl FnMut(&mut SimNet) -> bool,
    ) -> bool {
        let end = self.now + timeout;
        while self.now < end {
            if done(self) {
                return true;
            }
            self.step(tick);
        }
        done(self)
    }

    fn route(&mut self, from: usize, from_addr: SocketAddr, dest: SocketAddr, data: &[u8]) {
        let Some(to) = self.nodes.iter().position(|n| n.addr == dest) else {
            self.dropped += 1;
            return;
        };
        let Some(link) = self.links.get_mut(&(from, to)) else {
            self.dropped += 1;
            return;
        };
        if link.spec.loss > 0.0 && self.rng.random::<f64>() < link.spec.loss {
            self.dropped += 1;
            return;
        }
        let mut depart = self.now;
        #[allow(clippy::cast_precision_loss)] // sim rates and sizes are far below 2^52
        if let Some(bw) = link.spec.bandwidth_bytes_per_sec {
            let start = link.next_free.max(self.now);
            let queued_bytes = start.saturating_duration_since(self.now).as_secs_f64() * bw as f64;
            if queued_bytes > link.spec.queue_bytes as f64 {
                self.dropped += 1;
                return;
            }
            let serialization = Duration::from_secs_f64(data.len() as f64 / bw as f64);
            link.next_free = start + serialization;
            depart = link.next_free;
        }
        let jitter = if link.spec.jitter.is_zero() {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(self.rng.random::<f64>() * link.spec.jitter.as_secs_f64())
        };
        self.seq += 1;
        self.in_flight.push(Reverse(InFlight {
            arrive_at: depart + link.spec.delay + jitter,
            seq: self.seq,
            to,
            from_addr,
            data: data.to_vec(),
        }));
    }
}

#[cfg(test)]
mod tests;
