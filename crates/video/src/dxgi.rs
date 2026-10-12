//! Vendored DXGI Desktop Duplication capture (Windows only).
//!
//! Purpose-built replacement for xcap's `VideoRecorder`, written against
//! the same `windows` crate version xcap uses. What it fixes, learned
//! the hard way in the field:
//!
//! - **Joinable worker with an exit flag.** xcap's capture thread can
//!   only be parked, never stopped — a "stopped" recorder still holds
//!   the DXGI duplication, and Windows allows one per output per
//!   process, so every later session collides with the corpse.
//! - **Observable death.** xcap's `VideoRecorder` struct holds a clone
//!   of the channel Sender, so a dead capture thread can never be seen
//!   as a disconnect. Here the worker publishes health explicitly and
//!   rebuilds the duplication *itself* after `ACCESS_LOST` (display
//!   mode changes, fullscreen transitions, UAC), with backoff.
//! - **One CPU pass per frame instead of three.** The staging texture
//!   is created once and reused; rows are copied out honoring
//!   `RowPitch` into a recycled buffer (`crate::pool`) and delivered as
//!   **BGRA** exactly as the desktop stores them — no BGRA→RGBA swizzle
//!   (swscale takes BGRA directly, straight from this buffer) and no
//!   redundant clone.
//! - **Latest-frame slot instead of a rendezvous channel.** The worker
//!   never blocks on the consumer; the consumer takes the newest frame
//!   whenever it polls. Staleness is bounded by construction.
//! - **HDR detection.** An FP16 desktop (`R16G16B16A16_FLOAT`) would
//!   decode as garbage colors through a BGRA8 readback; it is reported
//!   as a permanent failure so the caller can fall back to screenshots
//!   (GDI tone-maps) instead of streaming noise.
//!
//! - **The pointer.** The desktop comes without it; where it is and
//!   what it looks like come beside each frame, and it is drawn into
//!   the copy (`crate::pointer`; review P2-25). A move with nothing
//!   else changing is a frame of its own: the desktop as last read,
//!   with the pointer where it now is.
//!
//! Frame pacing note: DXGI only delivers frames when the screen
//! *changes*; the desktop image present at duplication creation is
//! reported with `LastPresentTime == 0` and skipped (matching xcap) —
//! callers seed the first picture from a screenshot.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use windows::{
    core::{Interface, BOOL},
    Win32::{
        Foundation::{HMODULE, LPARAM, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
                ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_SINGLETHREADED,
                D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::DXGI_FORMAT_B8G8R8A8_UNORM, CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice,
                IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
                DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
            },
            Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
        },
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    },
};

use crate::{
    pointer::{Kind, Rect, Shape},
    CaptureError,
};

/// How long `AcquireNextFrame` waits before reporting "no change".
/// Also bounds worker shutdown latency (the exit flag is checked each
/// iteration).
const ACQUIRE_TIMEOUT_MS: u32 = 200;

/// …and how long while a pointer move is still owed a frame: it came
/// when the queue was full, and the consumer takes one every 16 or 33
/// ms. Without this the pointer's last position before it stopped could
/// reach the viewers a whole [`ACQUIRE_TIMEOUT_MS`] late.
const POINTER_OWED_TIMEOUT_MS: u32 = 8;

// NOTE: there is deliberately no rebuild loop in this module. Field
// evidence (see capture.rs) shows duplications created from the capture
// worker thread die instantly while a fullscreen app owns the output,
// while duplications created from the consumer thread work — so on any
// fatal error the worker reports Lost and exits, and the *consumer*
// recreates the whole duplicator on its own thread.

/// Frames buffered between worker and consumer. Depth 2 (not 1): a
/// 60 fps source on a higher-refresh display presents on an uneven
/// vsync cadence (60-in-144 alternates ~14/21 ms), so a fixed-rate
/// consumer against a single slot loses frames to phase jitter — the
/// tick finds the slot empty a hair early, and the *next* source frame
/// is then discarded because the slot is occupied (field-measured:
/// ~50 fresh/s from a 60 fps video). One extra slot absorbs the
/// misalignment at the cost of ≤1 frame of latency, while readback
/// stays demand-paced: the worker still skips the expensive copy
/// whenever the queue is full, so readback tracks consumption rather
/// than display refresh.
const FRAME_QUEUE_DEPTH: usize = 2;

/// One captured desktop frame, tightly packed **BGRA**, top-down.
pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// When it was read back from the desktop.
    pub at: Instant,
    /// Where the pointer was drawn into it, if it was: the one part of
    /// the picture a screenshot of the same desktop does not have.
    pub pointer: Option<Rect>,
}

/// Worker health as published for the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Capturing (or waiting for the screen to change).
    Running,
    /// Duplication lost (mode change etc.); the worker has exited.
    /// Drop this duplicator and create a new one **from the consumer
    /// thread** — see the module note on why the thread matters.
    Lost,
    /// Permanently failed (unsupported/HDR desktop format); see
    /// [`DxgiDuplicator::failure`]. Fall back to another backend.
    Failed,
}

const HEALTH_RUNNING: u8 = 0;
const HEALTH_LOST: u8 = 1;
const HEALTH_FAILED: u8 = 2;

struct Shared {
    exit: AtomicBool,
    health: AtomicU8,
    failure: Mutex<Option<String>>,
    /// Frame queue (depth [`FRAME_QUEUE_DEPTH`]): worker pushes,
    /// consumer pops oldest-first.
    slot: Mutex<VecDeque<CapturedFrame>>,
}

impl Shared {
    fn lost(&self, msg: String) {
        log::info!("dxgi capture: duplication lost ({msg}) — worker exiting for recreate");
        *self.failure.lock().unwrap() = Some(msg);
        self.health.store(HEALTH_LOST, Ordering::Release);
    }

    fn fail(&self, msg: String) {
        log::warn!("dxgi capture failed permanently: {msg}");
        *self.failure.lock().unwrap() = Some(msg);
        self.health.store(HEALTH_FAILED, Ordering::Release);
    }
}

/// Owns the capture worker for one monitor. Dropping it stops and
/// **joins** the worker — the duplication is fully released, so the
/// next session can recreate it without fighting a zombie.
pub struct DxgiDuplicator {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl DxgiDuplicator {
    /// Create the duplication for `monitor_index` (same ordering as
    /// [`crate::capture::list_monitors`]) and start the capture worker.
    ///
    /// Device and duplication are created synchronously on the caller
    /// so init failures (unsupported session, duplication limit, no
    /// such monitor) surface here, letting the caller fall back.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if the monitor can't be found or the
    /// OS refuses the duplication.
    pub fn new(monitor_index: usize) -> Result<Self, CaptureError> {
        let (device, context, duplication) = open_duplication(hmonitor_at(monitor_index)?)?;

        let shared = Arc::new(Shared {
            exit: AtomicBool::new(false),
            health: AtomicU8::new(HEALTH_RUNNING),
            failure: Mutex::new(None),
            slot: Mutex::new(VecDeque::with_capacity(FRAME_QUEUE_DEPTH)),
        });

        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("dxgi-capture".into())
            .spawn(move || worker_loop(worker_shared, monitor_index, device, context, duplication))
            .map_err(|e| CaptureError::Capture(format!("spawn capture worker: {e}")))?;

        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Take a buffered frame, if any arrived since the last call: the
    /// oldest — unless the desktop makes pictures well faster than one
    /// every `every` (the caller's own pace), when the older of two is
    /// a picture nobody needs and the newer goes out instead. A 30 fps
    /// share of a 60 Hz desktop found the queue full at every tick and
    /// always took the older one: a frame interval of the screen, 17 to
    /// 33 ms, late for nothing (review P3-28). At matched rates the
    /// oldest still goes first: that is what the second slot is for
    /// ([`FRAME_QUEUE_DEPTH`]).
    #[must_use]
    pub fn take_frame(&self, every: Duration) -> Option<CapturedFrame> {
        let mut queue = self.shared.slot.lock().unwrap();
        let surplus = match (queue.front(), queue.back()) {
            (Some(older), Some(newer)) if queue.len() > 1 => {
                crate::capture::source_outpaces(newer.at.duration_since(older.at), every)
            }
            _ => false,
        };
        if surplus && let Some(stale) = queue.pop_front() {
            crate::pool::recycle(stale.data);
        }
        queue.pop_front()
    }

    /// Current worker health.
    #[must_use]
    pub fn health(&self) -> Health {
        match self.shared.health.load(Ordering::Acquire) {
            HEALTH_RUNNING => Health::Running,
            HEALTH_LOST => Health::Lost,
            _ => Health::Failed,
        }
    }

    /// The loss/failure message, once not [`Health::Running`].
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.shared.failure.lock().unwrap().clone()
    }
}

impl Drop for DxgiDuplicator {
    fn drop(&mut self) {
        self.shared.exit.store(true, Ordering::Release);
        if let Some(handle) = self.worker.take() {
            // Bounded by ACQUIRE_TIMEOUT_MS + one frame of work.
            let _ = handle.join();
        }
    }
}

// ── Worker ──────────────────────────────────────────────────────────────

fn worker_loop(
    shared: Arc<Shared>,
    _monitor_index: usize,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
) {
    // Belt-and-braces COM init for DXGI interop from a bare thread.
    // Cheap, and one plausible mechanism for why worker-created
    // duplications behaved worse than consumer-created ones.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

    // Persistent staging texture, recreated only when the source
    // dimensions change (mode switches). It keeps the desktop as last
    // read, which is what a pointer move is drawn onto.
    let mut staging: Option<(ID3D11Texture2D, u32, u32)> = None;
    let mut pointer = Pointer::default();

    'outer: loop {
        if shared.exit.load(Ordering::Acquire) {
            break;
        }

        let owed = pointer.owed && staging.is_some();
        let timeout = if owed {
            POINTER_OWED_TIMEOUT_MS
        } else {
            ACQUIRE_TIMEOUT_MS
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired =
            unsafe { duplication.AcquireNextFrame(timeout, &raw mut info, &raw mut resource) };

        match acquired {
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                // No screen change within the timeout — normal on a
                // static desktop.
                let _ = unsafe { duplication.ReleaseFrame() };
            }
            Err(e) => {
                // ACCESS_LOST and friends: the duplication is dead
                // (mode changes, fullscreen transitions). Report and
                // exit — the consumer recreates the whole duplicator on
                // its own thread, where creation demonstrably works
                // even while a fullscreen app owns the output.
                let _ = unsafe { duplication.ReleaseFrame() };
                shared.lost(e.to_string());
                break 'outer;
            }
            Ok(()) => {
                // The pointer first: its shape can only be asked for
                // while the frame is held.
                unsafe { pointer.note(&duplication, &info) };

                // The frame present at duplication creation reports
                // LastPresentTime == 0 — skip it (callers seed the
                // first picture from a screenshot instead). So does one
                // in which only the pointer moved: nothing to read from
                // the desktop, and the pointer's frame is made below.
                if info.LastPresentTime == 0 {
                    let _ = unsafe { duplication.ReleaseFrame() };
                    unsafe { pointer_frame(&shared, &context, staging.as_ref(), &mut pointer) };
                    continue;
                }

                // Demand pacing: the desktop presents at display
                // refresh rate, but the consumer only takes frames at
                // the stream rate. If the queue is full, this frame
                // would outpace consumption — skip the expensive part
                // (GPU copy, synchronizing Map, multi-MB memcpy)
                // entirely and release. Acquire+release without
                // readback is nearly free, and readback throughput then
                // automatically matches consumption instead of the
                // display's refresh rate (a 144 Hz desktop feeding a
                // 30 fps stream would otherwise waste ~80% of its
                // readbacks). See FRAME_QUEUE_DEPTH for why the queue
                // is 2 deep rather than a single slot.
                if shared.slot.lock().unwrap().len() >= FRAME_QUEUE_DEPTH {
                    let _ = unsafe { duplication.ReleaseFrame() };
                    continue;
                }

                let grabbed =
                    unsafe { grab_frame(&device, &context, &duplication, resource, &mut staging) };
                match grabbed {
                    Ok(Some(mut frame)) => {
                        pointer.draw(&mut frame);
                        shared.slot.lock().unwrap().push_back(frame);
                    }
                    Ok(None) => {}
                    Err(GrabError::UnsupportedFormat(fmt)) => {
                        let _ = unsafe { duplication.ReleaseFrame() };
                        shared.fail(format!(
                            "desktop pixel format {fmt} is not BGRA8 — HDR desktops are not \
                             supported by the streaming backend (colors would be garbage); \
                             falling back to screenshots"
                        ));
                        break 'outer;
                    }
                    Err(GrabError::Windows(e)) => {
                        // Treat like a lost duplication: report + exit
                        // so the consumer recreates us.
                        let _ = unsafe { duplication.ReleaseFrame() };
                        shared.lost(format!("frame readback failed: {e}"));
                        break 'outer;
                    }
                }
                continue;
            }
        }

        // Nothing new from the desktop: a pointer move that found the
        // queue full is given its frame now, if there is room.
        unsafe { pointer_frame(&shared, &context, staging.as_ref(), &mut pointer) };
    }

    if com.is_ok() {
        unsafe { CoUninitialize() };
    }
}

// ── The pointer ─────────────────────────────────────────────────────────

/// The pointer as the duplication last described it.
#[derive(Default)]
struct Pointer {
    /// Its picture; `None` until the first one comes, or after one that
    /// did not fit its own description.
    shape: Option<Shape>,
    /// Top-left corner of the picture on this output (the hot spot is
    /// already taken off by the system).
    x: i32,
    y: i32,
    /// On this output and shown (not hidden by a game, not on another
    /// monitor).
    visible: bool,
    /// It moved, changed or went away since the last frame that was
    /// queued: the viewers are owed one.
    owed: bool,
    /// Where the system writes a new shape, kept between shapes.
    scratch: Vec<u8>,
}

impl Pointer {
    /// Take what this frame says about the pointer. Must be called
    /// before the frame is released.
    unsafe fn note(
        &mut self,
        duplication: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) {
        // The share's own switch (`capture::set_pointer_shown`), and a
        // frame that says nothing about the pointer.
        if !crate::capture::pointer_shown() {
            if self.visible {
                self.visible = false;
                self.owed = true;
            }
            return;
        }
        if info.LastMouseUpdateTime == 0 {
            return;
        }
        let at = info.PointerPosition.Position;
        let visible = info.PointerPosition.Visible.as_bool();
        if visible != self.visible || (visible && (at.x, at.y) != (self.x, self.y)) {
            self.owed = true;
        }
        self.visible = visible;
        (self.x, self.y) = (at.x, at.y);

        if info.PointerShapeBufferSize == 0 {
            return;
        }
        self.scratch.resize(info.PointerShapeBufferSize as usize, 0);
        let mut needed = 0u32;
        let mut described = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let read = unsafe {
            duplication.GetFramePointerShape(
                info.PointerShapeBufferSize,
                self.scratch.as_mut_ptr().cast(),
                &raw mut needed,
                &raw mut described,
            )
        };
        if let Err(e) = read {
            // The old shape, if any, is better than none.
            log::debug!("dxgi capture: pointer shape not read: {e}");
            return;
        }
        let kind = match described.Type {
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 => Some(Kind::Monochrome),
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 => Some(Kind::Color),
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 => {
                Some(Kind::MaskedColor)
            }
            _ => None,
        };
        let filled = (needed as usize).min(self.scratch.len());
        self.shape = kind.and_then(|kind| {
            Shape::new(
                kind,
                described.Width,
                described.Height,
                described.Pitch,
                &self.scratch[..filled],
            )
        });
        if self.shape.is_none() {
            log::debug!(
                "dxgi capture: a pointer shape that cannot be drawn (type {}, {}x{}, pitch {}, {} bytes)",
                described.Type,
                described.Width,
                described.Height,
                described.Pitch,
                filled
            );
        }
        self.owed = true;
    }

    /// Draw the pointer into a frame about to be queued, which settles
    /// what was owed.
    fn draw(&mut self, frame: &mut CapturedFrame) {
        self.owed = false;
        if !self.visible {
            return;
        }
        if let Some(shape) = &self.shape {
            frame.pointer = shape.draw(&mut frame.data, frame.width, frame.height, self.x, self.y);
        }
    }
}

/// The frame a pointer move is owed: the desktop as it was last read
/// (it has not changed since, or that would have been a frame), with
/// the pointer where it is now. Nothing if none is owed, the queue is
/// full (the next pass asks again) or the desktop was never read (the
/// first real frame will carry the pointer).
unsafe fn pointer_frame(
    shared: &Shared,
    context: &ID3D11DeviceContext,
    staging: Option<&(ID3D11Texture2D, u32, u32)>,
    pointer: &mut Pointer,
) {
    let Some(staging) = staging else {
        return;
    };
    if !pointer.owed || shared.slot.lock().unwrap().len() >= FRAME_QUEUE_DEPTH {
        return;
    }
    match unsafe { read_staging(context, staging) } {
        Ok(mut frame) => {
            pointer.draw(&mut frame);
            shared.slot.lock().unwrap().push_back(frame);
        }
        Err(e) => {
            // Not worth the duplication: the pointer waits for the
            // next frame of the desktop.
            pointer.owed = false;
            log::debug!("dxgi capture: the pointer's frame could not be read: {e}");
        }
    }
}

enum GrabError {
    UnsupportedFormat(i32),
    Windows(windows::core::Error),
}

impl From<windows::core::Error> for GrabError {
    fn from(e: windows::core::Error) -> Self {
        GrabError::Windows(e)
    }
}

/// Copy the acquired frame into CPU memory (tightly packed BGRA).
/// Releases the duplication frame as soon as the GPU copy is queued —
/// before the (synchronizing) Map — so the duplication is held for the
/// minimum time.
unsafe fn grab_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    duplication: &IDXGIOutputDuplication,
    resource: Option<IDXGIResource>,
    staging: &mut Option<(ID3D11Texture2D, u32, u32)>,
) -> Result<Option<CapturedFrame>, GrabError> {
    let Some(resource) = resource else {
        let _ = unsafe { duplication.ReleaseFrame() };
        return Ok(None);
    };
    let source: ID3D11Texture2D = resource.cast()?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&raw mut desc) };

    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return Err(GrabError::UnsupportedFormat(desc.Format.0));
    }
    let (width, height) = (desc.Width, desc.Height);

    // (Re)build the staging texture only when dimensions change.
    let rebuild = match staging {
        Some((_, w, h)) => *w != width || *h != height,
        None => true,
    };
    if rebuild {
        let mut staging_desc = desc;
        staging_desc.BindFlags = 0;
        staging_desc.MiscFlags = 0;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;

        let mut tex = None;
        unsafe { device.CreateTexture2D(&raw const staging_desc, None, Some(&raw mut tex)) }?;
        let tex = tex.ok_or_else(|| {
            GrabError::Windows(windows::core::Error::from_hresult(windows::core::HRESULT(
                -1,
            )))
        })?;
        *staging = Some((tex, width, height));
    }
    let staging = staging.as_ref().expect("built above");
    let (staging_tex, _, _) = staging;

    let region = D3D11_BOX {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
        front: 0,
        back: 1,
    };
    let staging_res: ID3D11Resource = staging_tex.cast()?;
    let source_res: ID3D11Resource = source.cast()?;
    unsafe {
        context.CopySubresourceRegion(
            Some(&staging_res),
            0,
            0,
            0,
            0,
            Some(&source_res),
            0,
            Some(&raw const region),
        );
    }

    // GPU copy is queued — the duplication frame is no longer needed.
    let _ = unsafe { duplication.ReleaseFrame() };

    Ok(Some(unsafe { read_staging(context, staging) }?))
}

/// Read the staging texture out into CPU memory (tightly packed BGRA):
/// the frame just copied into it, or — for a pointer move — the same
/// desktop once more.
unsafe fn read_staging(
    context: &ID3D11DeviceContext,
    staging: &(ID3D11Texture2D, u32, u32),
) -> Result<CapturedFrame, windows::core::Error> {
    let (staging_tex, width, height) = staging;
    let (width, height) = (*width, *height);
    let staging_res: ID3D11Resource = staging_tex.cast()?;

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.Map(
            Some(&staging_res),
            0,
            D3D11_MAP_READ,
            0,
            Some(&raw mut mapped),
        )
    }?;

    // A buffer from the frame pool, appended to row by row: a fresh
    // zero-filled one per frame was 8 MB at 1080p and 33 MB at 4K, every
    // page faulted in, at the capture rate (review P1-11). The consumer
    // gives back the frame this one replaces.
    let row_bytes = (width as usize) * 4;
    let mut data = crate::pool::take(row_bytes * height as usize);
    let src_ptr = mapped.pData as *const u8;
    for row in 0..height as usize {
        let src = unsafe {
            std::slice::from_raw_parts(src_ptr.add(row * mapped.RowPitch as usize), row_bytes)
        };
        data.extend_from_slice(src);
    }

    unsafe { context.Unmap(Some(&staging_res), 0) };

    Ok(CapturedFrame {
        data,
        width,
        height,
        at: Instant::now(),
        pointer: None,
    })
}

// ── Setup ───────────────────────────────────────────────────────────────

/// HMONITOR for a monitor index, enumerated with `EnumDisplayMonitors`
/// in the exact order xcap's `Monitor::all()` uses — indexes stay
/// aligned with [`crate::capture::list_monitors`] and the UI picker.
/// Returned as `isize` so it can cross into the worker thread.
fn hmonitor_at(monitor_index: usize) -> Result<isize, CaptureError> {
    unsafe extern "system" fn enum_proc(
        h_monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        state: LPARAM,
    ) -> BOOL {
        let list = unsafe { &mut *(state.0 as *mut Vec<isize>) };
        list.push(h_monitor.0 as isize);
        BOOL::from(true)
    }

    let mut list: Vec<isize> = Vec::new();
    unsafe {
        EnumDisplayMonitors(None, None, Some(enum_proc), LPARAM(&raw mut list as isize))
            .ok()
            .map_err(|e| CaptureError::MonitorList(e.to_string()))?;
    }

    list.get(monitor_index)
        .copied()
        .ok_or(CaptureError::MonitorNotFound(monitor_index))
}

/// D3D11 device + duplication for the given monitor. Same creation
/// sequence as xcap's recorder (known to work on the target machines),
/// with one fix: outputs are matched to the monitor *before* calling
/// `DuplicateOutput`, instead of duplicating every output while
/// searching.
fn open_duplication(
    h_monitor_raw: isize,
) -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication), CaptureError> {
    let werr = |what: &str, e: windows::core::Error| CaptureError::Capture(format!("{what}: {e}"));
    unsafe {
        // The adapter that drives this monitor. On a machine with two
        // GPUs — a laptop's panel on the integrated one, a second
        // monitor on the other card — it need not be the default
        // adapter, whose outputs then do not include the monitor: the
        // share fell back to GDI screenshots (review P2-26).
        let found = find_output(h_monitor_raw);
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED;
        let mut device = None;
        match &found {
            // A device on a named adapter takes no driver type.
            Some((adapter, _)) => D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                None,
            ),
            None => D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                None,
            ),
        }
        .map_err(|e| werr("D3D11CreateDevice", e))?;
        let device: ID3D11Device =
            device.ok_or_else(|| CaptureError::Capture("D3D11CreateDevice: no device".into()))?;

        let context = device
            .GetImmediateContext()
            .map_err(|e| werr("GetImmediateContext", e))?;
        let dxgi_device: IDXGIDevice = device.cast().map_err(|e| werr("cast IDXGIDevice", e))?;

        let output = match found {
            Some((_, output)) => output,
            // The factory would not say: the default adapter's outputs,
            // as before.
            None => {
                let adapter = dxgi_device
                    .GetAdapter()
                    .map_err(|e| werr("GetAdapter", e))?;
                let mut output_index = 0u32;
                loop {
                    let output = adapter
                        .EnumOutputs(output_index)
                        .map_err(|e| werr("monitor's DXGI output not found", e))?;
                    output_index += 1;
                    let desc = output.GetDesc().map_err(|e| werr("output GetDesc", e))?;
                    if desc.Monitor.0 as isize == h_monitor_raw {
                        break output;
                    }
                }
            }
        };

        let output1: IDXGIOutput1 = output.cast().map_err(|e| werr("cast IDXGIOutput1", e))?;
        let duplication = output1
            .DuplicateOutput(&dxgi_device)
            .map_err(|e| werr("DuplicateOutput", e))?;

        Ok((device, context, duplication))
    }
}

/// The adapter one of whose outputs is the monitor, with that output:
/// every adapter is asked, not only the default one. `None` when DXGI
/// lists no such output (or no adapters at all).
unsafe fn find_output(h_monitor_raw: isize) -> Option<(IDXGIAdapter1, IDXGIOutput)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut adapter_index = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        adapter_index += 1;
        let mut output_index = 0u32;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            output_index += 1;
            let ours = unsafe { output.GetDesc() }
                .is_ok_and(|desc| desc.Monitor.0 as isize == h_monitor_raw);
            if ours {
                return Some((adapter, output));
            }
        }
    }
    None
}
