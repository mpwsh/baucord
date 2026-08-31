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
//!   `RowPitch` and delivered as **BGRA** exactly as the desktop stores
//!   them — no BGRA→RGBA swizzle (swscale takes BGRA directly) and no
//!   redundant clone.
//! - **Latest-frame slot instead of a rendezvous channel.** The worker
//!   never blocks on the consumer; the consumer takes the newest frame
//!   whenever it polls. Staleness is bounded by construction.
//! - **HDR detection.** An FP16 desktop (`R16G16B16A16_FLOAT`) would
//!   decode as garbage colors through a BGRA8 readback; it is reported
//!   as a permanent failure so the caller can fall back to screenshots
//!   (GDI tone-maps) instead of streaming noise.
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
};

use windows::{
    core::{Interface, BOOL},
    Win32::{
        Foundation::{HMODULE, LPARAM, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
                ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_SINGLETHREADED,
                D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice, IDXGIOutput1,
                IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO,
            },
            Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
        },
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    },
};

use crate::CaptureError;

/// How long `AcquireNextFrame` waits before reporting "no change".
/// Also bounds worker shutdown latency (the exit flag is checked each
/// iteration).
const ACQUIRE_TIMEOUT_MS: u32 = 200;

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

    /// Take the oldest buffered frame, if any arrived since last call.
    pub fn take_frame(&self) -> Option<CapturedFrame> {
        self.shared.slot.lock().unwrap().pop_front()
    }

    /// Current worker health.
    pub fn health(&self) -> Health {
        match self.shared.health.load(Ordering::Acquire) {
            HEALTH_RUNNING => Health::Running,
            HEALTH_LOST => Health::Lost,
            _ => Health::Failed,
        }
    }

    /// The loss/failure message, once not [`Health::Running`].
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
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
) {
    // Belt-and-braces COM init for DXGI interop from a bare thread.
    // Cheap, and one plausible mechanism for why worker-created
    // duplications behaved worse than consumer-created ones.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

    // Persistent staging texture, recreated only when the source
    // dimensions change (mode switches).
    let mut staging: Option<(ID3D11Texture2D, u32, u32)> = None;

    'outer: loop {
        if shared.exit.load(Ordering::Acquire) {
            break;
        }

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let acquired =
            unsafe { duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut info, &mut resource) };

        match acquired {
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                // No screen change within the timeout — normal on a
                // static desktop.
                let _ = unsafe { duplication.ReleaseFrame() };
                continue;
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
                // The frame present at duplication creation reports
                // LastPresentTime == 0 — skip it (callers seed the
                // first picture from a screenshot instead).
                if info.LastPresentTime == 0 {
                    let _ = unsafe { duplication.ReleaseFrame() };
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
                    unsafe { grab_frame(&_device, &context, &duplication, resource, &mut staging) };
                match grabbed {
                    Ok(Some(frame)) => {
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
            }
        }
    }

    if com.is_ok() {
        unsafe { CoUninitialize() };
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
    let resource = match resource {
        Some(r) => r,
        None => {
            let _ = unsafe { duplication.ReleaseFrame() };
            return Ok(None);
        }
    };
    let source: ID3D11Texture2D = resource.cast()?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&mut desc) };

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
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut tex)) }?;
        let tex = tex.ok_or_else(|| {
            GrabError::Windows(windows::core::Error::from_hresult(windows::core::HRESULT(
                -1,
            )))
        })?;
        *staging = Some((tex, width, height));
    }
    let (staging_tex, _, _) = staging.as_ref().expect("built above");

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
            Some(&region),
        );
    }

    // GPU copy is queued — the duplication frame is no longer needed.
    let _ = unsafe { duplication.ReleaseFrame() };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(Some(&staging_res), 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;

    let mut data = vec![0u8; (width as usize) * (height as usize) * 4];
    let row_bytes = (width as usize) * 4;
    let src_ptr = mapped.pData as *const u8;
    for row in 0..height as usize {
        let src = unsafe {
            std::slice::from_raw_parts(src_ptr.add(row * mapped.RowPitch as usize), row_bytes)
        };
        data[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(src);
    }

    unsafe { context.Unmap(Some(&staging_res), 0) };

    Ok(Some(CapturedFrame {
        data,
        width,
        height,
    }))
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
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut list as *mut Vec<isize> as isize),
        )
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
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(|e| werr("D3D11CreateDevice", e))?;
        let device: ID3D11Device =
            device.ok_or_else(|| CaptureError::Capture("D3D11CreateDevice: no device".into()))?;

        let context = device
            .GetImmediateContext()
            .map_err(|e| werr("GetImmediateContext", e))?;
        let dxgi_device: IDXGIDevice = device.cast().map_err(|e| werr("cast IDXGIDevice", e))?;
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
            if desc.Monitor.0 as isize != h_monitor_raw {
                continue;
            }

            let output1: IDXGIOutput1 = output.cast().map_err(|e| werr("cast IDXGIOutput1", e))?;
            let duplication = output1
                .DuplicateOutput(&dxgi_device)
                .map_err(|e| werr("DuplicateOutput", e))?;

            return Ok((device, context, duplication));
        }
    }
}
