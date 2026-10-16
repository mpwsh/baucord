//! X11 screen capture that only reads the screen when it changed.
//!
//! xcap read the whole monitor on every capture tick: four new X
//! connections, a `GetImage` and a per-pixel conversion to RGBA, 30–60
//! times a second, after which the share loop compared it with the
//! picture before and, on a still screen, threw it away (review
//! 2026-10, P2-43). Here one connection stays open and the X server's
//! DAMAGE extension says where anything was drawn: no damage on this
//! monitor, no read. The pixels are the server's own 32-bit `BGRX`,
//! handed on as [`PixelFormat::Bgra`] for swscale to take as they are
//! (it reads no alpha).
//!
//! Not under Wayland: `XWayland`'s root window holds only X clients'
//! windows, so the caller keeps xcap's Wayland path there.

use std::time::Instant;

use x11rb::{
    connection::{Connection as _, RequestConnection as _},
    protocol::{
        damage::{self, ConnectionExt as _, ReportLevel},
        xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Window},
        Event,
    },
    rust_connection::RustConnection,
};

use crate::{capture::CaptureStatus, CaptureError, Frame, PixelFormat};

/// Is this an X11 session (and not Wayland, whose `XWayland` root window
/// shows only X clients)? Mirrors xcap's own test, which decides
/// which of its paths the fallback would take.
#[must_use]
pub fn is_x11_session() -> bool {
    let var = |name| std::env::var_os(name).unwrap_or_default();
    let wayland = var("XDG_SESSION_TYPE") == "wayland"
        || var("WAYLAND_DISPLAY")
            .to_string_lossy()
            .to_lowercase()
            .contains("wayland");
    !wayland && !var("DISPLAY").is_empty()
}

/// One monitor of an X11 screen, read when the server reports damage.
pub struct X11Capturer {
    conn: RustConnection,
    root: Window,
    damage: damage::Damage,
    /// The monitor's rectangle on the root window.
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    /// Something was drawn on this monitor since the last read (the
    /// first read counts).
    dirty: bool,
    start_time: Instant,
    last: Option<Frame>,
}

impl X11Capturer {
    /// Watch the monitor at `x, y` (`width × height`) on the default
    /// screen.
    ///
    /// # Errors
    ///
    /// No X server, no DAMAGE extension, or a pixel layout other than
    /// 32 bits per pixel, least significant byte first — the caller
    /// then uses xcap, which reads every layout.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, CaptureError> {
        let err = |what: &str, e: &dyn std::fmt::Display| {
            CaptureError::Capture(format!("X11 capture: {what}: {e}"))
        };
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| err("connect", &e))?;
        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or_else(|| CaptureError::Capture("X11 capture: no default screen".into()))?;
        let root = screen.root;
        let bpp = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == screen.root_depth)
            .map(|f| f.bits_per_pixel);
        if bpp != Some(32) || setup.image_byte_order != ImageOrder::LSB_FIRST {
            return Err(CaptureError::Capture(format!(
                "X11 capture: depth {} at {bpp:?} bits per pixel, {:?}: xcap reads it",
                screen.root_depth, setup.image_byte_order
            )));
        }
        if conn
            .extension_information(damage::X11_EXTENSION_NAME)
            .map_err(|e| err("query DAMAGE", &e))?
            .is_none()
        {
            return Err(CaptureError::Capture(
                "X11 capture: no DAMAGE extension".into(),
            ));
        }
        conn.damage_query_version(1, 1)
            .map_err(|e| err("DAMAGE version", &e))?
            .reply()
            .map_err(|e| err("DAMAGE version", &e))?;
        let damage = conn.generate_id().map_err(|e| err("id", &e))?;
        // Bounding box, not "non-empty": every event says where the
        // damage reaches, so damage on another monitor is told apart,
        // and damage here after it still reports.
        conn.damage_create(damage, root, ReportLevel::BOUNDING_BOX)
            .map_err(|e| err("DamageCreate", &e))?
            .check()
            .map_err(|e| err("DamageCreate", &e))?;
        let size = |v: u32| u16::try_from(v).map_err(|e| err("monitor size", &e));
        let place = |v: i32| i16::try_from(v).map_err(|e| err("monitor position", &e));
        Ok(Self {
            x: place(x)?,
            y: place(y)?,
            width: size(width)?,
            height: size(height)?,
            conn,
            root,
            damage,
            dirty: true,
            start_time: Instant::now(),
            last: None,
        })
    }

    /// Read the monitor if the server reported damage on it since the
    /// last read; otherwise [`CaptureStatus::Unchanged`] without
    /// touching the screen.
    ///
    /// # Errors
    ///
    /// The connection broke or the read failed.
    pub fn poll(&mut self) -> Result<CaptureStatus, CaptureError> {
        let err = |what: &str, e: &dyn std::fmt::Display| {
            CaptureError::Capture(format!("X11 capture: {what}: {e}"))
        };
        let mut reported = false;
        while let Some(event) = self.conn.poll_for_event().map_err(|e| err("events", &e))? {
            if let Event::DamageNotify(ev) = event {
                reported = true;
                self.dirty |= self.overlaps(ev.area);
            }
        }
        // Clear what was reported (before reading, so what is drawn
        // during the read reports again): the box starts over, and
        // the next drawing anywhere is an event.
        if reported {
            self.conn
                .damage_subtract(self.damage, x11rb::NONE, x11rb::NONE)
                .map_err(|e| err("DamageSubtract", &e))?;
            self.conn.flush().map_err(|e| err("flush", &e))?;
        }
        if !self.dirty {
            return Ok(if self.last.is_some() {
                CaptureStatus::Unchanged
            } else {
                CaptureStatus::Empty
            });
        }
        self.dirty = false;
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                self.x,
                self.y,
                self.width,
                self.height,
                u32::MAX,
            )
            .map_err(|e| err("GetImage", &e))?
            .reply()
            .map_err(|e| err("GetImage", &e))?;
        let (width, height) = (u32::from(self.width), u32::from(self.height));
        let data = reply.data;
        if data.len() != width as usize * height as usize * 4 {
            return Err(CaptureError::Capture(format!(
                "X11 capture: {} bytes for {width}x{height}",
                data.len()
            )));
        }
        let frame = Frame {
            data,
            width,
            height,
            timestamp_ms: self.start_time.elapsed().as_millis() as u64,
            format: PixelFormat::Bgra,
        };
        Ok(crate::capture::hold_if_changed(&mut self.last, frame))
    }

    /// Does the root-window rectangle `area` reach this monitor?
    fn overlaps(&self, area: x11rb::protocol::xproto::Rectangle) -> bool {
        let span = |a: i16, al: u16, b: i16, bl: u16| {
            i32::from(a) < i32::from(b) + i32::from(bl)
                && i32::from(b) < i32::from(a) + i32::from(al)
        };
        span(area.x, area.width, self.x, self.width)
            && span(area.y, area.height, self.y, self.height)
    }

    /// See [`crate::capture::Capturer::frame`].
    pub fn frame(&mut self, advance_timestamp: bool) -> Option<&Frame> {
        if advance_timestamp && let Some(f) = &mut self.last {
            f.timestamp_ms = self.start_time.elapsed().as_millis() as u64;
        }
        self.last.as_ref()
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        let _ = self.conn.damage_destroy(self.damage);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use x11rb::{
        protocol::xproto::{ChangeGCAux, CreateGCAux, Rectangle},
        wrapper::ConnectionExt as _,
    };

    use super::*;

    /// Runs against a real X server (`xvfb-run -a -s "-noreset" cargo
    /// test -p video x11`: without `-noreset` Xvfb starts over each time
    /// its last client leaves, and a connection made then is refused);
    /// passes untested without one.
    #[test]
    fn reads_only_after_damage_on_its_monitor() {
        if !is_x11_session() {
            return;
        }
        let (conn, screen_num) = x11rb::connect(None).unwrap();
        let screen = &conn.setup().roots[screen_num];
        let (root, w, h) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);
        // The left half is "the monitor".
        let half = w / 2;
        let mut cap = X11Capturer::new(0, 0, u32::from(half), u32::from(h)).unwrap();
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Fresh);
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Unchanged);

        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(0x00ff_0000))
            .unwrap();
        let paint = |x: i16, colour: u32| {
            conn.change_gc(gc, &ChangeGCAux::new().foreground(colour))
                .unwrap();
            let rect = Rectangle {
                x,
                y: 10,
                width: 20,
                height: 20,
            };
            conn.poly_fill_rectangle(root, gc, &[rect]).unwrap();
            conn.sync().unwrap();
            // The damage event is on its way to the capturer's socket.
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        // Drawn on the other half: no read.
        paint((half + 10) as i16, 0x00ff_0000);
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Unchanged);
        // Drawn here: read, and seen.
        paint(10, 0x0000_ff00);
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Fresh);
        let f = cap.frame(false).unwrap();
        assert_eq!((f.width, f.format), (u32::from(half), PixelFormat::Bgra));
        let at = (15 * f.width as usize + 15) * 4;
        assert_eq!(&f.data[at..at + 3], &[0, 0xff, 0], "BGR of green");
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Unchanged);
        // Drawn here with the same pixels: read, and found the same.
        paint(10, 0x0000_ff00);
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Unchanged);
    }

    #[test]
    fn an_x11_session_shares_through_damage() {
        if !is_x11_session() {
            return;
        }
        let mut cap = crate::capture::Capturer::new(0, crate::Quality::HD30).unwrap();
        assert_eq!(cap.backend_name(), "X11 (GetImage on DAMAGE)");
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Fresh);
        assert_eq!(cap.poll().unwrap(), CaptureStatus::Unchanged);
    }
}
