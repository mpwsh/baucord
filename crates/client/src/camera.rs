//! Our camera (1.5): capture → encode → the network thread, on a thread
//! of its own.
//!
//! Started and stopped by the UI ([`CameraControl`]); idle, the thread
//! sleeps on its control channel. Running, it is paced by the camera:
//! each [`video::camera::Camera::frame`] waits for the device's next
//! picture, which is encoded as H.264 (the share's encoder, at the
//! size Settings → Camera asks — 720p by default — or the camera's own
//! when smaller) and handed to the network thread as
//! [`MediaOut::CameraFrame`] for the camera's own uplink track. A
//! viewer's keyframe request forces the next frame to be one; the
//! relay's `CameraHealth` steps the picture down a size at a time
//! while viewers lose — never below [`Quality::CAMERA_FLOOR`] — and
//! back up once they have not for a while.
//!
//! The device belongs to this thread (some backends cannot move
//! between threads), so it is opened here, not handed in.
//!
//! The self-view: a smaller copy of every other frame goes into
//! [`CameraShared::preview`], newest only, and the UI is woken to take
//! it. Settings → Camera's preview is the same thread without the
//! encoder (`Start { send: false }`).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use log::{info, warn};
use video::{EncoderPreference, Quality, VideoEncoder};

use crate::network::MediaOut;
use crate::WakeFn;

/// What the UI tells the camera thread.
#[derive(Debug)]
pub enum CameraControl {
    /// Open a camera — `device` by its name, the first one when it is
    /// `None` or gone — and, with `send`, encode and send it; without,
    /// only the self-view runs (Settings → Camera's preview).
    Start {
        device: Option<String>,
        encoder: EncoderPreference,
        send: bool,
        /// The self-view reads as a mirror.
        mirror: bool,
        /// The size to send (`Quality::CAMERA`).
        quality: Quality,
    },
    /// Close it.
    Stop,
    /// Settings → Camera changed what the open camera does.
    Options {
        mirror: bool,
        quality: Quality,
    },
    /// The sizes a camera can deliver (Settings → Camera's list):
    /// answered with [`CameraEvent::Heights`].
    Probe {
        device: Option<String>,
    },
    /// A viewer asked for a keyframe (`CameraKeyframeRequested`).
    ForceKeyframe,
    /// The relay's account of our camera's viewers (`CameraHealth`).
    Health {
        worst_loss: f32,
        viewers: u16,
    },
    Shutdown,
}

/// What the camera thread tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum CameraEvent {
    /// Frames are flowing at this size: time to tell the server. The
    /// rest is for the telemetry line (`TelemetryEvent::CameraStarted`).
    Started {
        width: u16,
        height: u16,
        camera: String,
        mode: String,
        encoder: String,
    },
    /// The camera closed (asked, or it failed after `Error`).
    Stopped,
    /// It could not start or stopped working.
    Error(CameraFailure),
    /// A `Probe`'s answer: the camera's heights, tallest first (empty:
    /// it would not say, or there is none).
    Heights(Vec<u32>),
}

/// Why a camera did not start or stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraFailure {
    NoCamera,
    NotAllowed,
    Unsupported,
    NoEncoder,
    Other(String),
}

/// The camera to open: `device` by its name, else the first.
fn pick_camera(device: Option<&str>) -> Result<u32, CameraFailure> {
    let cameras = video::camera::list_cameras().map_err(|e| failure(&e))?;
    let first = cameras.first().ok_or(CameraFailure::NoCamera)?.index;
    Ok(device
        .and_then(|name| cameras.iter().find(|c| c.name == name))
        .map_or(first, |c| c.index))
}

/// The newest self-view picture: RGBA, its width and height.
pub type Preview = (Vec<u8>, u32, u32);

/// Between the thread and the UI.
#[derive(Default)]
pub struct CameraShared {
    pub preview: Mutex<Option<Preview>>,
}

pub struct CameraThreadHandle {
    pub tx: Sender<CameraControl>,
    pub event_rx: Receiver<CameraEvent>,
    pub shared: Arc<CameraShared>,
}

impl CameraThreadHandle {
    /// Start the thread; it sleeps until the first `Start`.
    #[must_use]
    pub fn spawn(media_tx: Sender<MediaOut>, wake: WakeFn) -> Self {
        let (tx, rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let shared = Arc::new(CameraShared::default());
        let thread_shared = Arc::clone(&shared);
        let _ = thread::Builder::new()
            .name("camera".into())
            .spawn(move || run(&rx, &event_tx, &media_tx, &thread_shared, &wake));
        Self {
            tx,
            event_rx,
            shared,
        }
    }
}

/// Health reports (one a second) in a row with loss before stepping
/// down, and without before stepping back up.
const LOSSY_REPORTS_DOWN: u32 = 3;
const CLEAN_REPORTS_UP: u32 = 15;
/// What counts as loss in a report: a viewer behind on a twentieth.
const LOSSY: f32 = 0.05;
/// The self-view's bound, and how often it is refreshed (every n-th
/// frame: 12 a second at 24).
// A tile is small, but most screens are dense: the self-view is drawn
// from up to 640×360 so it does not look soft beside the others.
const PREVIEW_MAX: (u32, u32) = (640, 360);
const PREVIEW_EVERY: u64 = 2;

fn failure(e: &video::camera::CameraError) -> CameraFailure {
    use video::camera::CameraError as E;
    match e {
        E::NoCamera => CameraFailure::NoCamera,
        E::NotAllowed => CameraFailure::NotAllowed,
        E::Unsupported => CameraFailure::Unsupported,
        E::Backend(s) | E::Decode(s) => CameraFailure::Other(s.clone()),
    }
}

/// The step-down ladder, apart from the thread so it can be tested.
/// It moves one size at a time between `top` (what was asked, or the
/// camera's own when smaller) and the floor (480p, or `top` when that
/// is already below it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ladder {
    quality: Quality,
    top: Quality,
    lossy: u32,
    clean: u32,
}

/// The next size down or up in [`Quality::CAMERA`].
fn neighbour(q: Quality, down: bool) -> Option<Quality> {
    let all = Quality::CAMERA;
    let i = all.iter().position(|c| *c == q)?;
    if down {
        all.get(i + 1).copied()
    } else {
        i.checked_sub(1).and_then(|j| all.get(j).copied())
    }
}

impl Ladder {
    const fn new(top: Quality) -> Self {
        Self {
            quality: top,
            top,
            lossy: 0,
            clean: 0,
        }
    }

    fn floor(&self) -> u32 {
        Quality::CAMERA_FLOOR.height().min(self.top.height())
    }

    /// One health report; the quality to switch to, if it changes.
    fn report(&mut self, worst_loss: f32, viewers: u16) -> Option<Quality> {
        if viewers == 0 {
            return None;
        }
        if worst_loss >= LOSSY {
            self.lossy += 1;
            self.clean = 0;
        } else {
            self.clean += 1;
            self.lossy = 0;
        }
        let next = if self.lossy >= LOSSY_REPORTS_DOWN {
            neighbour(self.quality, true).filter(|q| q.height() >= self.floor())
        } else if self.clean >= CLEAN_REPORTS_UP && self.quality != self.top {
            neighbour(self.quality, false)
        } else {
            None
        }?;
        self.quality = next;
        self.lossy = 0;
        self.clean = 0;
        Some(next)
    }
}

struct Running {
    camera: Box<dyn video::camera::Camera>,
    /// What it was opened by (Settings → Camera's pick).
    device: Option<String>,
    /// `None`: the preview alone, nothing is sent.
    encoder: Option<Box<dyn VideoEncoder + Send>>,
    mirror: bool,
    preference: EncoderPreference,
    ladder: Ladder,
    seq: u32,
    frames: u64,
    announced: bool,
    started: Instant,
    dropped: u64,
}

fn encoder_for(
    quality: Quality,
    preference: EncoderPreference,
) -> Result<Box<dyn VideoEncoder + Send>, CameraFailure> {
    match video::codec::FfmpegEncoder::new_with(quality, preference, &[]) {
        Ok(enc) => {
            info!("camera: encoding with {} at {quality}", enc.encoder_name());
            ENCODER_NAME.with(|n| n.set(enc.encoder_name()));
            Ok(Box::new(enc))
        }
        Err(e) => {
            warn!("camera: no encoder: {e}");
            Err(CameraFailure::NoEncoder)
        }
    }
}

thread_local! {
    /// The encoder the camera thread opened last, for telemetry.
    static ENCODER_NAME: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

struct StartWith {
    device: Option<String>,
    preference: EncoderPreference,
    send: bool,
    mirror: bool,
    quality: Quality,
}

/// What a camera that delivers `height` can send when `wanted` is
/// asked.
fn top_for(wanted: Quality, height: u32) -> Quality {
    let camera = Quality::camera_for_height(height);
    if camera.height() < wanted.height() {
        camera
    } else {
        wanted
    }
}

fn start(with: &StartWith) -> Result<Running, CameraFailure> {
    let index = pick_camera(with.device.as_deref())?;
    let camera = video::camera::open(index, with.quality.height()).map_err(|e| failure(&e))?;
    // Never above what the camera delivers: the encoder does not
    // upscale, and a 720p label on a 480p picture would be a lie.
    let ladder = Ladder::new(top_for(with.quality, camera.size().1));
    info!("camera: sending at most {}", ladder.top);
    let encoder = if with.send {
        Some(encoder_for(ladder.quality, with.preference)?)
    } else {
        None
    };
    Ok(Running {
        camera,
        device: with.device.clone(),
        encoder,
        mirror: with.mirror,
        preference: with.preference,
        ladder,
        seq: 0,
        frames: 0,
        announced: false,
        started: Instant::now(),
        dropped: 0,
    })
}

fn stop(running: Option<Running>, event_tx: &Sender<CameraEvent>) {
    if let Some(r) = running {
        info!(
            "camera: closed after {} s, {} frames sent, {} dropped",
            r.started.elapsed().as_secs(),
            r.frames,
            r.dropped
        );
        let _ = event_tx.send(CameraEvent::Stopped);
    }
}

/// The camera closes for good after a failure: the thread stops and
/// the UI is told why.
fn fail(
    running: &mut Option<Running>,
    event_tx: &Sender<CameraEvent>,
    wake: &WakeFn,
    f: CameraFailure,
) {
    stop(running.take(), event_tx);
    let _ = event_tx.send(CameraEvent::Error(f));
    wake();
}

/// A new encoder at `quality` for a camera that is sending.
fn reencode(r: &mut Running, quality: Quality) -> Result<(), CameraFailure> {
    if r.encoder.is_some() {
        r.encoder = Some(encoder_for(quality, r.preference)?);
    }
    Ok(())
}

/// One command. `false`: the thread ends.
fn handle(
    control: CameraControl,
    running: &mut Option<Running>,
    event_tx: &Sender<CameraEvent>,
    shared: &CameraShared,
    wake: &WakeFn,
) -> bool {
    match control {
        CameraControl::Start {
            device,
            encoder,
            send,
            mirror,
            quality,
        } => {
            stop(running.take(), event_tx);
            match start(&StartWith {
                device,
                preference: encoder,
                send,
                mirror,
                quality,
            }) {
                Ok(r) => *running = Some(r),
                Err(f) => {
                    warn!("camera: could not start: {f:?}");
                    fail(running, event_tx, wake, f);
                }
            }
        }
        CameraControl::Stop => {
            stop(running.take(), event_tx);
            if let Ok(mut p) = shared.preview.lock() {
                *p = None;
            }
            wake();
        }
        CameraControl::Options { mirror, quality } => {
            let Some(r) = running.as_mut() else {
                return true;
            };
            r.mirror = mirror;
            let top = top_for(quality, r.camera.size().1);
            if top != r.ladder.top {
                // A bigger size than the device was opened at needs it
                // opened again; a smaller one is the encoder's alone.
                if top.height() > r.camera.size().1 {
                    let with = StartWith {
                        device: r.device.clone(),
                        preference: r.preference,
                        send: r.encoder.is_some(),
                        mirror,
                        quality,
                    };
                    stop(running.take(), event_tx);
                    match start(&with) {
                        Ok(r) => *running = Some(r),
                        Err(f) => fail(running, event_tx, wake, f),
                    }
                    return true;
                }
                r.ladder = Ladder::new(top);
                if let Err(f) = reencode(r, top) {
                    fail(running, event_tx, wake, f);
                }
            }
        }
        CameraControl::Probe { device } => {
            let heights = match running.as_mut() {
                Some(r) if r.device == device => r.camera.heights(),
                _ => pick_camera(device.as_deref())
                    .ok()
                    .and_then(|i| video::camera::heights(i).ok())
                    .unwrap_or_default(),
            };
            let _ = event_tx.send(CameraEvent::Heights(heights));
            wake();
        }
        CameraControl::ForceKeyframe => {
            if let Some(enc) = running.as_mut().and_then(|r| r.encoder.as_mut()) {
                enc.force_keyframe();
            }
        }
        CameraControl::Health {
            worst_loss,
            viewers,
        } => {
            let Some(r) = running.as_mut().filter(|r| r.encoder.is_some()) else {
                return true;
            };
            if let Some(q) = r.ladder.report(worst_loss, viewers) {
                info!("camera: viewers' loss {worst_loss:.2}, now {q}");
                if let Err(f) = reencode(r, q) {
                    fail(running, event_tx, wake, f);
                }
            }
        }
        CameraControl::Shutdown => {
            stop(running.take(), event_tx);
            return false;
        }
    }
    true
}

fn run(
    rx: &Receiver<CameraControl>,
    event_tx: &Sender<CameraEvent>,
    media_tx: &Sender<MediaOut>,
    shared: &CameraShared,
    wake: &WakeFn,
) {
    let mut running: Option<Running> = None;
    loop {
        // Idle: wait for a command. Running: take what has come, then
        // the next frame (the camera paces the loop).
        let next = if running.is_some() {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };
        if let Some(control) = next {
            if !handle(control, &mut running, event_tx, shared, wake) {
                return;
            }
            continue;
        }
        let Some(r) = &mut running else {
            continue;
        };
        if let Err(f) = step(r, event_tx, media_tx, shared, wake) {
            warn!("camera: stopped working: {f:?}");
            fail(&mut running, event_tx, wake, f);
        }
    }
}

/// One picture: capture, the self-view, encode, send.
#[allow(clippy::cast_possible_truncation)]
fn step(
    r: &mut Running,
    event_tx: &Sender<CameraEvent>,
    media_tx: &Sender<MediaOut>,
    shared: &CameraShared,
    wake: &WakeFn,
) -> Result<(), CameraFailure> {
    let frame = r.camera.frame().map_err(|e| failure(&e))?;
    r.frames += 1;
    if r.frames % PREVIEW_EVERY == 1 {
        let mut small = video::camera::downscale_rgba(&frame, PREVIEW_MAX.0, PREVIEW_MAX.1);
        if r.mirror {
            mirror(&mut small);
        }
        if let Ok(mut p) = shared.preview.lock() {
            *p = Some(small);
        }
        wake();
    }
    let Some(encoder) = r.encoder.as_mut() else {
        return Ok(());
    };
    let encoded = encoder
        .encode(&frame)
        .map_err(|e| CameraFailure::Other(e.to_string()))?;
    for e in encoded {
        if !r.announced {
            // The server is told the size the viewers will decode.
            r.announced = true;
            let _ = event_tx.send(CameraEvent::Started {
                width: e.width.min(u32::from(u16::MAX)) as u16,
                height: e.height.min(u32::from(u16::MAX)) as u16,
                camera: r.camera.name().to_string(),
                mode: format!("{}, sending {}", r.camera.mode(), r.ladder.quality),
                encoder: ENCODER_NAME.with(std::cell::Cell::get).to_string(),
            });
            wake();
        }
        let seq = r.seq;
        r.seq = r.seq.wrapping_add(1);
        let sent = media_tx.try_send(MediaOut::CameraFrame {
            seq,
            is_keyframe: e.is_keyframe,
            width: e.width as u16,
            height: e.height as u16,
            timestamp_ms: e.timestamp_ms as u32,
            data: e.data,
        });
        if sent.is_err() {
            // The network thread is behind: the viewers see the gap and
            // ask for a keyframe.
            r.dropped += 1;
        }
    }
    Ok(())
}

/// The self-view reads as a mirror, as every video call shows it; what
/// is sent is not mirrored.
fn mirror((pixels, width, _): &mut Preview) {
    let row = *width as usize * 4;
    if row == 0 {
        return;
    }
    for line in pixels.chunks_exact_mut(row) {
        let px = line.len() / 4;
        for x in 0..px / 2 {
            let (a, b) = (x * 4, (px - 1 - x) * 4);
            for k in 0..4 {
                line.swap(a + k, b + k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_self_view_is_mirrored() {
        let mut p: Preview = (vec![1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3], 3, 1);
        mirror(&mut p);
        assert_eq!(p.0, vec![3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn loss_steps_down_a_size_at_a_time_to_480p_and_back_up() {
        let mut l = Ladder::new(Quality::Cam1080);
        for _ in 0..2 {
            assert_eq!(l.report(0.2, 2), None);
        }
        assert_eq!(l.report(0.2, 2), Some(Quality::Cam720));
        for _ in 0..2 {
            assert_eq!(l.report(0.2, 2), None);
        }
        assert_eq!(l.report(0.2, 2), Some(Quality::Cam480));
        // At the floor: no further.
        for _ in 0..10 {
            assert_eq!(l.report(0.9, 2), None);
        }
        assert_eq!(l.quality, Quality::Cam480);
        for _ in 0..14 {
            assert_eq!(l.report(0.0, 2), None);
        }
        assert_eq!(l.report(0.0, 2), Some(Quality::Cam720));
        for _ in 0..14 {
            l.report(0.0, 2);
        }
        assert_eq!(l.report(0.0, 2), Some(Quality::Cam1080));
        for _ in 0..30 {
            assert_eq!(l.report(0.0, 2), None);
        }
    }

    #[test]
    fn a_240p_camera_stays_at_240p_and_a_small_camera_caps_the_size() {
        let mut l = Ladder::new(Quality::Cam240);
        for _ in 0..10 {
            assert_eq!(l.report(0.9, 2), None);
        }
        assert_eq!(top_for(Quality::Cam1080, 720), Quality::Cam720);
        assert_eq!(top_for(Quality::Cam720, 1080), Quality::Cam720);
        assert_eq!(top_for(Quality::Cam720, 240), Quality::Cam240);
    }

    #[test]
    fn three_lossy_reports_step_down_and_fifteen_clean_ones_step_back() {
        let mut l = Ladder::new(Quality::Cam720);
        assert_eq!(l.report(0.2, 2), None);
        assert_eq!(l.report(0.2, 2), None);
        assert_eq!(l.report(0.2, 2), Some(Quality::Cam480));
        for _ in 0..14 {
            assert_eq!(l.report(0.0, 2), None);
        }
        assert_eq!(l.report(0.0, 2), Some(Quality::Cam720));
    }

    #[test]
    fn a_clean_report_breaks_a_lossy_run_and_nobody_watching_counts_for_nothing() {
        let mut l = Ladder::new(Quality::Cam720);
        l.report(0.2, 1);
        l.report(0.2, 1);
        assert_eq!(l.report(0.0, 1), None);
        assert_eq!(l.report(0.2, 1), None);
        assert_eq!(l.report(0.9, 0), None);
        assert_eq!(l.quality, Quality::Cam720);
    }
}
