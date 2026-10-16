//! Present-prep thread: the stage between "decoded" and "on screen".
//!
//! Field finding that started it: the sharer encodes 60 fps and the
//! viewer *decodes* ~60 fps, but frames used to reach the screen at
//! ~30 — the UI thread paid a 5-8 ms full-frame pixel copy per frame
//! (on top of Slint's own render + the rest of the drain), pushing each
//! present cycle past one frame period and coalescing every other
//! frame away.
//!
//! This thread takes ownership of the screen thread's event channel and
//! splits it:
//!
//! - `FrameReady` events: of everything queued, only the newest frame
//!   of each stream is prepared — converted from the decoder's YUV
//!   planes (Android) or copied (desktop RGBA) straight into the
//!   [`slint::SharedPixelBuffer`] the UI will draw from — and it lands
//!   in a newest-per-stream slot map. The UI thread wraps the buffer in
//!   a `slint::Image` (a cheap handle, not a copy) and sets a property:
//!   well under a millisecond.
//! - Every other event is forwarded untouched for the normal UI-thread
//!   handler.
//!
//! Only the frame that will be shown is converted (MOB-18, 2026-10-08
//! phone log): a mid-range phone's hardware decoded 1080p60 in 7–10 ms
//! a frame, then spent 24–30 ms converting every picture to RGBA on the
//! decode thread, one core. The decoder fell behind and the picture
//! drifted seconds behind its sound. Now the decode thread only copies
//! the planes out; this thread converts on a few cores, and when it
//! still cannot keep up it skips to the newest picture — the frame rate
//! drops, the picture stays live and in step with the audio.
//!
//! No per-frame allocation: each stream keeps a small ring of the pixel
//! buffers it handed to the UI and fills the oldest again (a fresh 8 MB
//! buffer is zero-filled, then faulted in page by page — a second full
//! write per frame), and the decoder's buffers go back to
//! [`video::pool`].

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use client::{
    screen::{FrameMeta, PresentTallies, ScreenControl, ScreenEvent, ScreenTx},
    WakeFn,
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use log::{error, info};
use video::PictureFormat;

type Pixels = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

/// The screen thread's control channel, for telling it who can see a
/// stream: the host's visibility comes from any thread
/// (`app::set_host_visible`), a viewer window's from its own hooks.
static SCREEN_TX: OnceLock<ScreenTx> = OnceLock::new();

/// Called once, with the screen thread's sender, when the workers start.
/// The thread starts out decoding; a background reached before this
/// only costs what it always did (decoded frames the code below drops).
pub(crate) fn attach_screen(tx: ScreenTx) {
    let _ = SCREEN_TX.set(tx);
}

/// Tell the screen thread whether a stream's picture can be seen:
/// `None` for the whole app (a phone's background), a peer for its
/// viewer window (minimized). Unseen frames are not decoded at all; a
/// stream seen again asks for a keyframe (review P1-7). A blocking
/// send: a lost "visible" would leave the picture frozen, and the
/// screen thread drains its channel constantly.
pub(crate) fn set_stream_visible(client_id: Option<u64>, visible: bool) {
    if let Some(id) = client_id {
        client::network::set_stream_hidden(id, !visible);
    }
    if let Some(tx) = SCREEN_TX.get() {
        let _ = tx.send(ScreenControl::SetVisible { client_id, visible });
    }
}

/// Pixel buffers kept per stream. The UI holds the one it shows and
/// the slot may hold the next, so the oldest of three is normally free
/// to fill. Filling one still on screen cannot tear — Slint copies a
/// shared buffer before handing out its bytes — it only costs that
/// copy.
const RING: usize = 3;

/// A stream with no frame for this long gives its buffers back (it
/// stopped, or nobody watches it any more).
const FORGET_AFTER: Duration = Duration::from_secs(3);

/// A decoded frame whose pixels already live in a Slint-ready buffer.
/// Everything the UI needs to present + log, minus any heavy work.
pub struct PreparedFrame {
    pub meta: FrameMeta,
    pub buffer: Pixels,
}

/// Shared between the prep thread (producer) and the UI drain
/// (consumer).
#[derive(Default)]
pub struct PresentShared {
    /// Newest prepared frame per stream. The UI drain takes the whole
    /// map each pass.
    pub slots: Mutex<HashMap<u64, PreparedFrame>>,
}

/// Spawn the prep thread. Takes over `event_rx` (the UI must stop
/// reading it); non-frame events come back out on `forward_tx`.
/// `tallies` collects, per stream, the frames prepared (and the time
/// that took) and the frames passed over for a newer one, for the
/// stream's summary line (`client::screen::PresentTally`).
pub fn spawn(
    event_rx: Receiver<ScreenEvent>,
    forward_tx: Sender<ScreenEvent>,
    shared: Arc<PresentShared>,
    tallies: PresentTallies,
    wake: WakeFn,
) {
    let _ = std::thread::Builder::new()
        .name("present-prep".into())
        .spawn(move || {
            let mut prep = Prep::new(shared, tallies);
            let mut newest: HashMap<u64, Waiting> = HashMap::new();
            while let Some(first) = prep.next_event(&event_rx) {
                // Everything queued by now, keeping only the newest
                // frame of each stream: the others are never converted.
                let mut forwarded = false;
                let mut next = Some(first);
                while let Some(evt) = next {
                    match evt {
                        ScreenEvent::FrameReady {
                            meta,
                            pixels,
                            format,
                        } => prep.keep_newest(
                            &mut newest,
                            Waiting {
                                meta,
                                pixels,
                                format,
                            },
                        ),
                        // A stopped stream's buffers go by themselves,
                        // `FORGET_AFTER` without a frame.
                        other => {
                            if forward_tx.send(other).is_err() {
                                return;
                            }
                            forwarded = true;
                        }
                    }
                    next = event_rx.try_recv().ok();
                }
                // Forwarded events first: they need no work here.
                if forwarded {
                    wake();
                }
                if !newest.is_empty() {
                    let mut any = false;
                    for (_, frame) in newest.drain() {
                        any |= prep.prepare(frame);
                    }
                    if any {
                        wake();
                    }
                }
                prep.forget_idle(Instant::now());
            }
            // Channel closed: screen thread is gone (shutdown).
        });
}

/// A decoded frame waiting to be prepared.
struct Waiting {
    meta: FrameMeta,
    pixels: Vec<u8>,
    format: PictureFormat,
}

/// One stream's pixel buffers, oldest first.
struct Ring {
    buffers: VecDeque<Pixels>,
    used: Instant,
}

struct Prep {
    shared: Arc<PresentShared>,
    tallies: PresentTallies,
    rings: HashMap<u64, Ring>,
    /// Streams whose last frame could not be prepared: said once until
    /// one can be again.
    failing: HashSet<u64>,
    /// Cores a YUV conversion is spread over (`video::yuv::to_rgba_into`).
    threads: usize,
    /// Their parked threads: started by the first YUV picture (only
    /// Android's `MediaCodec` hands those over), ended with the last
    /// stream's buffers.
    bands: Option<video::yuv::BandPool>,
}

impl Prep {
    fn new(shared: Arc<PresentShared>, tallies: PresentTallies) -> Self {
        // Half the cores, at most four: the decoder, the audio, the
        // network and the UI's own rendering need the rest. A phone's
        // eight make four, which brings a ~25 ms 1080p conversion under
        // a 60 fps frame's 16.
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let threads = (cores / 2).clamp(1, 4);
        info!("present: up to {threads} threads per picture conversion ({cores} cores)");
        Self {
            shared,
            tallies,
            rings: HashMap::new(),
            failing: HashSet::new(),
            threads,
            bands: None,
        }
    }

    /// The next event, waiting for one. While buffers are held, wakes
    /// now and then to give back those of streams that went quiet.
    /// `None` once the screen thread is gone.
    fn next_event(&mut self, rx: &Receiver<ScreenEvent>) -> Option<ScreenEvent> {
        loop {
            if self.rings.is_empty() {
                return rx.recv().ok();
            }
            match rx.recv_timeout(FORGET_AFTER) {
                Ok(evt) => return Some(evt),
                Err(RecvTimeoutError::Timeout) => self.forget_idle(Instant::now()),
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Queue `frame` as its stream's newest, passing over the one it
    /// replaces before anyone paid to convert it.
    fn keep_newest(&self, newest: &mut HashMap<u64, Waiting>, frame: Waiting) {
        let id = frame.meta.client_id;
        if let Some(older) = newest.insert(id, frame) {
            video::pool::recycle(older.pixels);
            if let Ok(mut tallies) = self.tallies.lock() {
                tallies.entry(id).or_default().replaced += 1;
            }
        }
    }

    /// Convert or copy `frame` into one of its stream's pixel buffers
    /// and leave it in the UI's slot. True when there is a frame to
    /// show.
    fn prepare(&mut self, frame: Waiting) -> bool {
        let Waiting {
            meta,
            pixels,
            format,
        } = frame;
        let FrameMeta {
            client_id,
            width,
            height,
            seq,
            ..
        } = meta;
        // A phone app in the background shows nothing: no conversion,
        // no redraw. The screen thread stops decoding once it hears
        // (`set_stream_visible`); this catches what was already on its
        // way.
        if !crate::app::host_visible() {
            video::pool::recycle(pixels);
            return false;
        }

        let started = Instant::now();
        let mut buffer = self.buffer_for(client_id, width, height);
        if matches!(format, PictureFormat::Yuv420 { .. }) && self.bands.is_none() {
            self.bands = video::yuv::BandPool::new(self.threads);
        }
        let filled = fill(
            buffer.make_mut_bytes(),
            &pixels,
            format,
            self.bands.as_ref(),
        );
        video::pool::recycle(pixels);
        if let Err(e) = filled {
            if self.failing.insert(client_id) {
                error!(
                    "present: cannot show {width}x{height} frame seq={seq} from {client_id}: {e}"
                );
            }
            return false;
        }
        self.failing.remove(&client_id);
        let prepare_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        if let Some(ring) = self.rings.get_mut(&client_id) {
            ring.buffers.push_back(buffer.clone());
        }

        let prepared = PreparedFrame { meta, buffer };
        let replaced = self
            .shared
            .slots
            .lock()
            .unwrap()
            .insert(client_id, prepared)
            .is_some();
        if let Ok(mut tallies) = self.tallies.lock() {
            let tally = tallies.entry(client_id).or_default();
            tally.prepared += 1;
            tally.prepare_us = tally.prepare_us.saturating_add(prepare_us);
            tally.replaced += u32::from(replaced);
        }
        true
    }

    /// A buffer of the right size for `id`'s next frame: its oldest
    /// when the ring is full, else a new one (the first frames, and
    /// after a size change).
    fn buffer_for(&mut self, id: u64, width: u32, height: u32) -> Pixels {
        let ring = self.rings.entry(id).or_insert_with(|| Ring {
            buffers: VecDeque::with_capacity(RING),
            used: Instant::now(),
        });
        ring.used = Instant::now();
        ring.buffers
            .retain(|b| b.width() == width && b.height() == height);
        if ring.buffers.len() >= RING
            && let Some(buffer) = ring.buffers.pop_front()
        {
            return buffer;
        }
        Pixels::new(width, height)
    }

    fn forget_idle(&mut self, now: Instant) {
        self.rings
            .retain(|_, ring| now.duration_since(ring.used) < FORGET_AFTER);
        let rings = &self.rings;
        self.failing.retain(|id| rings.contains_key(id));
        if self.rings.is_empty() {
            self.bands = None;
        }
    }
}

/// Write `pixels` (laid out as `format`) into `out`, the UI buffer's
/// `width × height × 4` bytes.
fn fill(
    out: &mut [u8],
    pixels: &[u8],
    format: PictureFormat,
    bands: Option<&video::yuv::BandPool>,
) -> Result<(), String> {
    match format {
        PictureFormat::Rgba => {
            if pixels.len() != out.len() {
                return Err(format!(
                    "{} bytes of RGBA for a {}-byte picture",
                    pixels.len(),
                    out.len()
                ));
            }
            out.copy_from_slice(pixels);
            Ok(())
        }
        PictureFormat::Bgra => video::yuv::bgra_to_rgba_into(pixels, out),
        PictureFormat::Yuv420 { layout, color } => {
            video::yuv::to_rgba_into(pixels, &layout, color, out, bands)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_is_copied_and_a_wrong_size_refused() {
        let mut out = vec![0u8; 16];
        let pixels: Vec<u8> = (0..16).collect();
        fill(&mut out, &pixels, PictureFormat::Rgba, None).unwrap();
        assert_eq!(out, pixels);
        assert!(fill(&mut out, &pixels[..12], PictureFormat::Rgba, None).is_err());
    }

    /// iOS's decoder hands BGRA on; it becomes RGBA as it is written
    /// into the picture the UI draws (review P2-43).
    #[test]
    fn bgra_is_swapped_into_the_ui_buffer() {
        let pixels = [1u8, 2, 3, 0, 4, 5, 6, 0];
        let mut out = vec![0u8; 8];
        fill(&mut out, &pixels, PictureFormat::Bgra, None).unwrap();
        assert_eq!(out, [3, 2, 1, 255, 6, 5, 4, 255]);
        assert!(fill(&mut out, &pixels[..4], PictureFormat::Bgra, None).is_err());
    }

    #[test]
    fn yuv_is_converted_into_the_ui_buffer() {
        // 4x2 I420, mid-grey: every pixel comes out grey and opaque.
        let layout = video::yuv::Yuv420Layout::planar(4, 2, 4, 2);
        let src = vec![128u8; layout.required_len()];
        let mut out = vec![0u8; 4 * 2 * 4];
        fill(
            &mut out,
            &src,
            PictureFormat::Yuv420 {
                layout,
                color: video::yuv::ColorSpec::default(),
            },
            video::yuv::BandPool::new(4).as_ref(),
        )
        .unwrap();
        for px in out.chunks_exact(4) {
            assert_eq!(px[3], 255);
            assert!(px[..3].iter().all(|&c| (120..=140).contains(&c)), "{px:?}");
        }
    }

    fn prep() -> Prep {
        Prep::new(Arc::default(), PresentTallies::default())
    }

    fn frame(id: u64, fill_byte: u8) -> Waiting {
        let now = Instant::now();
        Waiting {
            meta: FrameMeta {
                client_id: id,
                width: 2,
                height: 2,
                frame_bytes: 100,
                seq: u32::from(fill_byte),
                enqueued_at: now,
                decoded_at: now,
                buffer_depth_ms: 0.0,
                buffer_target_ms: 0.0,
                buffer_jitter_ms: 0.0,
            },
            pixels: vec![fill_byte; 16],
            format: PictureFormat::Rgba,
        }
    }

    #[test]
    fn only_the_newest_queued_frame_is_prepared() {
        let mut p = prep();
        let mut newest = HashMap::new();
        for b in 1..=3 {
            p.keep_newest(&mut newest, frame(7, b));
        }
        assert_eq!(newest.len(), 1);
        for (_, f) in newest.drain() {
            assert!(p.prepare(f));
        }
        let slots = p.shared.slots.lock().unwrap();
        let shown = slots.get(&7).expect("prepared");
        assert_eq!(shown.meta.seq, 3);
        assert!(shown.buffer.as_bytes().iter().all(|&b| b == 3));
        let tally = p.tallies.lock().unwrap()[&7];
        assert_eq!((tally.prepared, tally.replaced), (1, 2));
    }

    #[test]
    fn buffers_go_round_once_the_ui_lets_go() {
        let mut p = prep();
        let mut first_ptr = None;
        for b in 1..=(RING as u8 + 1) {
            assert!(p.prepare(frame(7, b)));
            // The UI takes the frame and drops the one it showed.
            let taken = p.shared.slots.lock().unwrap().remove(&7).unwrap();
            if b == 1 {
                first_ptr = Some(taken.buffer.as_bytes().as_ptr());
            }
            if b == RING as u8 + 1 {
                assert_eq!(
                    Some(taken.buffer.as_bytes().as_ptr()),
                    first_ptr,
                    "the oldest buffer is filled again"
                );
                assert!(taken.buffer.as_bytes().iter().all(|&x| x == b));
            }
        }
        assert_eq!(p.rings[&7].buffers.len(), RING);
    }

    #[test]
    fn a_buffer_still_on_screen_is_never_overwritten() {
        let mut p = prep();
        // The UI keeps showing frame 1 the whole time.
        assert!(p.prepare(frame(7, 1)));
        let on_screen = p.shared.slots.lock().unwrap().remove(&7).unwrap();
        for b in 2..=(RING as u8 + 2) {
            assert!(p.prepare(frame(7, b)));
        }
        assert!(on_screen.buffer.as_bytes().iter().all(|&x| x == 1));
    }

    #[test]
    fn a_new_size_starts_a_new_ring_and_quiet_streams_are_forgotten() {
        let mut p = prep();
        for b in 1..=3 {
            assert!(p.prepare(frame(7, b)));
        }
        let mut big = frame(7, 9);
        big.meta.width = 4;
        big.pixels = vec![9; 4 * 2 * 4];
        assert!(p.prepare(big));
        assert_eq!(p.rings[&7].buffers.len(), 1);
        p.forget_idle(Instant::now() + FORGET_AFTER);
        assert!(p.rings.is_empty());
    }
}
