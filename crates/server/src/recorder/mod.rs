//! Voice channel recording (1.5): the thread that turns relayed voice
//! into a WAV, and the tick's handle to it.
//!
//! The tick only copies packets: the relay hands each one forwarded in
//! a recording channel to [`Recorder::packet`] (a `Bytes` clone and a
//! `try_send` on a bounded queue). This thread decodes, mixes
//! ([`mix::Mixer`]), writes the spool file ([`wav::WavWriter`]), and at
//! the end hashes it and moves it into the blob store. The thread is
//! started by the first recording and blocks on its queues: an idle one
//! never wakes. A full packet queue drops and counts — voice is never
//! delayed for a recording.

pub mod mix;
pub mod wav;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, select_biased, unbounded, Receiver, Sender, TrySendError};
use log::{info, warn};
use protocol::{Bytes, ClientId};

/// Packets waiting for the thread at most; past it they are dropped.
pub const PACKET_QUEUE: usize = 8_192;

/// Where a recording's spool lives while it runs.
#[must_use]
pub fn spool_path(spool: &Path, rec_id: u64) -> PathBuf {
    spool.join(format!("{rec_id}.wav.part"))
}

enum Control {
    Open {
        rec_id: u64,
        path: PathBuf,
        rate: u32,
        start: Instant,
    },
    /// The end: write what is left, then (with `blobs`) hash the file
    /// and move it into the blob store; without, only patch its header
    /// (a shutdown: the next start finishes it).
    Close {
        rec_id: u64,
        end: Instant,
        blobs: Option<PathBuf>,
    },
}

struct Packet {
    rec_id: u64,
    sender: ClientId,
    at: Instant,
    bytes: Bytes,
}

/// What the thread tells the tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecEvent {
    /// The file is in the blob store.
    Finished {
        rec_id: u64,
        blob_id: [u8; 32],
        size: u64,
        duration_ms: u64,
    },
    /// The file was closed where it was (a shutdown).
    Parked { rec_id: u64 },
    /// Writing failed: the disk is full, or the WAV reached its 4 GiB
    /// limit. The tick ends the recording; what was written is kept.
    WriteFailed { rec_id: u64, error: String },
    /// Nothing could be made of it.
    Failed { rec_id: u64, error: String },
}

/// The tick's handle. Every method returns at once.
pub struct Recorder {
    control: Option<Sender<Control>>,
    packets: Option<Sender<Packet>>,
    events: Receiver<RecEvent>,
    events_tx: Sender<RecEvent>,
    handle: Option<JoinHandle<()>>,
    /// Packets dropped because the queue was full.
    pub dropped: u64,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    #[must_use]
    pub fn new() -> Self {
        let (events_tx, events) = unbounded();
        Self {
            control: None,
            packets: None,
            events,
            events_tx,
            handle: None,
            dropped: 0,
        }
    }

    fn ensure_thread(&mut self) {
        if self.control.is_some() {
            return;
        }
        let (control_tx, control_rx) = unbounded();
        let (packets_tx, packets_rx) = bounded(PACKET_QUEUE);
        let events = self.events_tx.clone();
        match std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || run(&control_rx, &packets_rx, &events))
        {
            Ok(handle) => {
                self.handle = Some(handle);
                self.control = Some(control_tx);
                self.packets = Some(packets_tx);
            }
            Err(e) => warn!("the recorder thread did not start: {e}"),
        }
    }

    /// Begin a recording spooled at `path`.
    pub fn open(&mut self, rec_id: u64, path: PathBuf, rate: u32, start: Instant) {
        self.ensure_thread();
        let sent = self.control.as_ref().is_some_and(|c| {
            c.send(Control::Open {
                rec_id,
                path,
                rate,
                start,
            })
            .is_ok()
        });
        if !sent {
            let _ = self.events_tx.send(RecEvent::Failed {
                rec_id,
                error: "the recorder thread is not running".into(),
            });
        }
    }

    /// One relayed voice packet of a recording.
    pub fn packet(&mut self, rec_id: u64, sender: ClientId, at: Instant, bytes: Bytes) {
        let Some(tx) = &self.packets else {
            return;
        };
        match tx.try_send(Packet {
            rec_id,
            sender,
            at,
            bytes,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => self.dropped += 1,
        }
    }

    /// End a recording at `end`; with `blobs`, finish its file into
    /// that blob store.
    pub fn close(&mut self, rec_id: u64, end: Instant, blobs: Option<PathBuf>) {
        if let Some(c) = &self.control {
            let _ = c.send(Control::Close { rec_id, end, blobs });
        }
    }

    /// What the thread has said since the last call.
    pub fn poll(&mut self) -> Vec<RecEvent> {
        self.events.try_iter().collect()
    }

    /// Whether the thread runs.
    #[must_use]
    pub fn running(&self) -> bool {
        self.handle.is_some()
    }

    /// Stop the thread once what is queued is written, waiting at most
    /// `budget`. Recordings still open are parked: their spool's header
    /// is patched and the next start finishes them.
    pub fn shutdown(&mut self, budget: Duration) -> Vec<RecEvent> {
        self.control = None;
        self.packets = None;
        let deadline = Instant::now() + budget;
        if let Some(handle) = self.handle.take() {
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                warn!("the recorder did not finish within {budget:?}; its spools are repaired at the next start");
            }
        }
        self.poll()
    }
}

struct Open {
    mixer: mix::Mixer,
    writer: Option<wav::WavWriter>,
    path: PathBuf,
    rate: u32,
    start: Instant,
    since_flush: u64,
    failed: bool,
}

impl Open {
    fn pos(&self, at: Instant) -> u64 {
        let ns = at.saturating_duration_since(self.start).as_nanos();
        u64::try_from(ns * u128::from(self.rate) / 1_000_000_000).unwrap_or(u64::MAX)
    }

    fn write(&mut self, out: &[i16], rec_id: u64, events: &Sender<RecEvent>) {
        if out.is_empty() || self.failed {
            return;
        }
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        let result = w.write(out).and_then(|()| {
            self.since_flush += out.len() as u64;
            // A crash loses at most five seconds.
            if self.since_flush >= u64::from(self.rate) * 5 {
                self.since_flush = 0;
                w.flush()
            } else {
                Ok(())
            }
        });
        if let Err(e) = result {
            self.failed = true;
            let _ = events.send(RecEvent::WriteFailed {
                rec_id,
                error: e.to_string(),
            });
        }
    }
}

fn run(control: &Receiver<Control>, packets: &Receiver<Packet>, events: &Sender<RecEvent>) {
    let mut open: HashMap<u64, Open> = HashMap::new();
    let mut out: Vec<i16> = Vec::with_capacity(48_000);
    loop {
        select_biased! {
            recv(control) -> msg => match msg {
                Ok(Control::Open { rec_id, path, rate, start }) => {
                    let writer = match wav::WavWriter::create(&path, rate) {
                        Ok(w) => Some(w),
                        Err(e) => {
                            let _ = events.send(RecEvent::Failed { rec_id, error: e.to_string() });
                            None
                        }
                    };
                    if writer.is_some() {
                        open.insert(rec_id, Open {
                            mixer: mix::Mixer::new(rate),
                            writer,
                            path,
                            rate,
                            start,
                            since_flush: 0,
                            failed: false,
                        });
                    }
                }
                Ok(Control::Close { rec_id, end, blobs }) => {
                    // The packets queued before the close are its last.
                    for p in packets.try_iter() {
                        feed(&mut open, &p, &mut out, events);
                    }
                    if let Some(rec) = open.remove(&rec_id) {
                        close(rec_id, rec, end, blobs.as_deref(), &mut out, events);
                    }
                }
                Err(_) => break,
            },
            recv(packets) -> p => match p {
                Ok(p) => feed(&mut open, &p, &mut out, events),
                Err(_) => break,
            },
        }
    }
    // Shut down: park whatever is open.
    for p in packets.try_iter() {
        feed(&mut open, &p, &mut out, events);
    }
    for (rec_id, rec) in open.drain() {
        let end = Instant::now();
        close(rec_id, rec, end, None, &mut out, events);
    }
}

fn feed(open: &mut HashMap<u64, Open>, p: &Packet, out: &mut Vec<i16>, events: &Sender<RecEvent>) {
    let Some(rec) = open.get_mut(&p.rec_id) else {
        return;
    };
    let pos = rec.pos(p.at);
    rec.mixer.push(p.sender, pos, &p.bytes);
    out.clear();
    rec.mixer.drain_ready(out);
    rec.write(out, p.rec_id, events);
}

fn close(
    rec_id: u64,
    mut rec: Open,
    end: Instant,
    blobs: Option<&Path>,
    out: &mut Vec<i16>,
    events: &Sender<RecEvent>,
) {
    let end_pos = rec.pos(end).max(rec.mixer.written());
    out.clear();
    rec.mixer.finish(end_pos, out);
    rec.write(out, rec_id, events);
    let stats = rec.mixer.stats;
    let Some(writer) = rec.writer.take() else {
        return;
    };
    let samples = writer.samples();
    let size = match writer.finish() {
        Ok(size) => size,
        Err(e) => {
            let _ = events.send(RecEvent::Failed {
                rec_id,
                error: e.to_string(),
            });
            return;
        }
    };
    let duration_ms = samples * 1000 / u64::from(rec.rate.max(1));
    info!(
        "recording {rec_id}: {} packets mixed, {} repaired from RED, {} from FEC, {} concealed, \
         {} lost, {} too late; {size} bytes",
        stats.packets,
        stats.repaired_red,
        stats.repaired_fec,
        stats.concealed,
        stats.lost,
        stats.too_late
    );
    let Some(blobs) = blobs else {
        let _ = events.send(RecEvent::Parked { rec_id });
        return;
    };
    match wav::move_to_blobs(&rec.path, blobs) {
        Ok((blob_id, size)) => {
            let _ = events.send(RecEvent::Finished {
                rec_id,
                blob_id,
                size,
                duration_ms,
            });
        }
        Err(e) => {
            let _ = events.send(RecEvent::Failed {
                rec_id,
                error: e.to_string(),
            });
        }
    }
}
