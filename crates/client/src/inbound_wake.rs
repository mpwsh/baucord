//! Waking the network thread when a datagram arrives, so a connected
//! client sleeps instead of polling its socket every millisecond
//! (IOS-SPEC.md §14 D41, ANDROID-SPEC.md D43).
//!
//! The network thread blocks on its control channel: outbound voice,
//! video and commands wake it at once. Inbound datagrams could not —
//! the socket is not a channel — so the loop used to wake every 1 ms
//! to look, a thousand times a second for as long as a session lasted,
//! in a quiet voice channel or a chat-only session alike. On a phone
//! that alone keeps a CPU core from ever reaching its deep idle states.
//!
//! Here a small thread blocks in `poll(2)` on a duplicate of the
//! socket's descriptor and rings a channel the loop selects on, then
//! waits to be re-armed: the loop re-arms it once it has drained the
//! socket, so a datagram is announced once, never again while it waits
//! to be read (readiness is level-triggered). Unix only; Windows keeps
//! the 1 ms poll (`start` returns `None`), as does any platform where
//! the duplicate cannot be made.

#[cfg(unix)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crossbeam_channel::Receiver;
#[cfg(unix)]
use crossbeam_channel::Sender;
use transport::UdpIo;

/// How long the watcher sleeps in `poll` before checking whether its
/// session ended (it also ends when the loop drops the re-arm side).
#[cfg(unix)]
const WATCH_SLICE: std::time::Duration = std::time::Duration::from_millis(500);

/// The loop's side of the watcher.
#[cfg(unix)]
pub(crate) struct InboundWake {
    ready_rx: Receiver<()>,
    rearm_tx: Sender<()>,
    stop: Arc<AtomicBool>,
    /// A ring was taken; the watcher waits for the re-arm.
    owed: bool,
}

#[cfg(unix)]
impl InboundWake {
    /// Start watching `udp`. `None` where this cannot be done: the
    /// caller polls on a short timer instead.
    pub(crate) fn start(udp: &UdpIo) -> Option<Self> {
        let waiter = match udp.read_waiter() {
            Ok(w) => w,
            Err(e) => {
                log::warn!("inbound wake unavailable ({e}); polling the socket instead");
                return None;
            }
        };
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (rearm_tx, rearm_rx) = crossbeam_channel::bounded::<()>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_watch = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("net-inbound".into())
            .spawn(move || loop {
                if stop_watch.load(Ordering::Relaxed) {
                    return;
                }
                match waiter.wait(WATCH_SLICE) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        // The loop notices the closed channel and goes
                        // back to polling.
                        log::warn!("inbound wake stopped: {e}");
                        return;
                    }
                }
                if ready_tx.send(()).is_err() || rearm_rx.recv().is_err() {
                    return;
                }
            });
        if let Err(e) = spawned {
            log::warn!("inbound wake thread refused ({e}); polling the socket instead");
            return None;
        }
        Some(Self {
            ready_rx,
            rearm_tx,
            stop,
            owed: false,
        })
    }

    /// What the loop selects on beside its control channel.
    pub(crate) fn ready(&self) -> &Receiver<()> {
        &self.ready_rx
    }

    /// The loop took a ring.
    pub(crate) fn took(&mut self) {
        self.owed = true;
    }

    /// The loop has drained the socket: let the watcher look again.
    pub(crate) fn drained(&mut self) {
        if std::mem::take(&mut self.owed) {
            let _ = self.rearm_tx.try_send(());
        }
    }
}

#[cfg(unix)]
impl Drop for InboundWake {
    fn drop(&mut self) {
        // The watcher leaves at its next look (≤ WATCH_SLICE), or at
        // once if it is waiting for a re-arm that will never come.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Off Unix there is no watcher: the type has no values, `start` says
/// so, and the loop polls the socket every millisecond as before.
#[cfg(not(unix))]
pub(crate) enum InboundWake {}

#[cfg(not(unix))]
impl InboundWake {
    pub(crate) fn start(_udp: &UdpIo) -> Option<Self> {
        None
    }

    pub(crate) fn ready(&self) -> &Receiver<()> {
        match *self {}
    }

    pub(crate) fn took(&mut self) {
        match *self {}
    }

    pub(crate) fn drained(&mut self) {
        match *self {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn socket() -> UdpIo {
        UdpIo::bind("127.0.0.1:0".parse().unwrap(), 1 << 20).expect("bind")
    }

    /// One ring per arrival: nothing until a datagram lands; then one
    /// ring, and no other until the loop has drained and re-armed —
    /// however long the datagram waits.
    #[test]
    fn rings_once_per_drain() {
        let mut rx = socket();
        let mut wake = InboundWake::start(&rx).expect("unix");
        assert!(wake
            .ready()
            .recv_timeout(Duration::from_millis(60))
            .is_err());

        let tx = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind tx");
        let to = rx.local_addr().expect("addr");
        tx.send_to(&[1; 16], to).expect("send");
        wake.ready()
            .recv_timeout(Duration::from_secs(2))
            .expect("a ring for the datagram");
        wake.took();
        // Not drained yet: the watcher waits for the re-arm.
        assert!(wake
            .ready()
            .recv_timeout(Duration::from_millis(60))
            .is_err());

        let mut got = 0;
        rx.recv_batch(|_| got += 1).expect("recv");
        assert_eq!(got, 1);
        wake.drained();
        // Drained and re-armed, nothing new: quiet.
        assert!(wake
            .ready()
            .recv_timeout(Duration::from_millis(60))
            .is_err());

        tx.send_to(&[2; 16], to).expect("send");
        let start = Instant::now();
        wake.ready()
            .recv_timeout(Duration::from_secs(2))
            .expect("a ring for the next datagram");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    /// Dropping the loop's side ends the watcher (it holds a duplicate
    /// of the socket, which must not outlive the session for long).
    #[test]
    fn the_watcher_ends_with_the_session() {
        let rx = socket();
        let wake = InboundWake::start(&rx).expect("unix");
        let ready = wake.ready().clone();
        drop(wake);
        drop(rx);
        // The sender side goes away when the thread returns.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match ready.recv_timeout(Duration::from_millis(50)) {
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                _ if Instant::now() > deadline => panic!("watcher still running"),
                _ => {}
            }
        }
    }
}
