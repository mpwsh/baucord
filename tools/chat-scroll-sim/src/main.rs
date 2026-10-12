//! The chat pane's "stick to the newest message", checked the way it
//! fails: the app's own `ChatPane`, drawn frame by frame by Slint's
//! software renderer on a mocked 60 Hz clock, fed real wheel, scrollbar
//! and touch events and the host's appends. Each step prints whether
//! the pane follows new rows and how far it is from the end; any wrong
//! answer fails the run (exit 1).
//!
//!   cargo run --manifest-path tools/chat-scroll-sim/Cargo.toml
//!
//! `SHOT=<dir>` instead writes two screenshots (scrolled up with the
//! new-messages pill, and back at the end) as PPM. `CHAT_DIR=<dir>`
//! builds against another copy of crates/ui/slint, `JUMP` naming what
//! its jump button called (the 1.2 test builds before this fix, with
//! `JUMP='pane.scroll-to-end()'`, fail 9 of the 48 checks).
slint::include_modules!();

use std::{cell::Cell, rc::Rc, time::Duration};

use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType},
        Platform, PointerEventButton, WindowAdapter, WindowEvent,
    },
    ComponentHandle, LogicalPosition, Model, ModelRc, VecModel,
};

thread_local! { static NOW: Cell<u64> = const { Cell::new(0) }; }

struct P {
    window: Rc<MinimalSoftwareWindow>,
}
impl Platform for P {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        Duration::from_millis(NOW.with(Cell::get))
    }
}

struct Sim {
    win: Rc<MinimalSoftwareWindow>,
    ui: TestWindow,
    model: Rc<VecModel<ChatRow>>,
    buf: Vec<PremultipliedRgbaColor>,
    next: i32,
    /// The host holds the front of the list for a reader above the
    /// newest rows (`ChatHistory::hold_front`), as of its last append.
    held: bool,
    failures: Vec<String>,
}

/// `app_core::chat::MAX_ROWS` and `HELD_MAX_ROWS`: what an append trims
/// the list to, from the front — the second while the front is held.
const MAX_ROWS: usize = 500;
const HELD_MAX_ROWS: usize = 4 * MAX_ROWS;

impl Sim {
    fn frame(&mut self) {
        NOW.with(|n| n.set(n.get() + 16));
        slint::platform::update_timers_and_animations();
        self.win.request_redraw();
        self.win.draw_if_needed(|r| {
            r.render(&mut self.buf, 400);
        });
    }
    fn shot(&mut self, path: &str) {
        self.win.request_redraw();
        let mut buf = vec![PremultipliedRgbaColor::default(); 400 * 640];
        self.win.draw_if_needed(|r| {
            r.render(&mut buf, 400);
        });
        let mut out = format!("P6\n400 640\n255\n").into_bytes();
        for p in &buf {
            out.extend_from_slice(&[p.red, p.green, p.blue]);
        }
        std::fs::write(path, out).unwrap();
    }
    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.frame();
        }
    }
    fn row(&mut self, tall: bool, own: bool) -> ChatRow {
        self.next += 1;
        let i = self.next;
        let mut r = ChatRow::default();
        r.row_id = i;
        r.msg_id_lo = i;
        r.ts = "12:00".into();
        r.user = if own {
            "me".into()
        } else {
            format!("user{}", i % 4).into()
        };
        r.initial = "U".into();
        r.own = own;
        if tall {
            r.kind = 2;
            r.img_w = 400.0;
            r.img_h = 300.0 + f32::from(u8::try_from(i % 7).unwrap()) * 20.0;
        } else {
            r.kind = 0;
            let words = 3 + (i * 37 % 60);
            r.body = (0..words)
                .map(|w| format!("word{w}"))
                .collect::<Vec<_>>()
                .join(" ")
                .into();
        }
        r
    }
    /// An append as the host does it with the cap (`ChatHistory::cap`,
    /// then `sync_history_tail`): rows over the limit leave from the
    /// front first, the new ones are pushed, and whether the reader
    /// followed decides if the front is held at the *next* append.
    /// Returns how many rows left.
    fn append_capped(&mut self, n: usize, tall_every: usize) -> usize {
        let limit = if self.held { HELD_MAX_ROWS } else { MAX_ROWS };
        let dropped = (self.model.row_count() + n).saturating_sub(limit);
        for _ in 0..dropped {
            self.model.remove(0);
        }
        for k in 0..n {
            let tall = tall_every > 0 && k % tall_every == tall_every - 1;
            let r = self.row(tall, false);
            self.model.push(r);
        }
        let following = self.ui.invoke_appended();
        self.held = !following;
        if !following {
            let s = self.ui.global::<State>();
            s.set_chat_new_count(s.get_chat_new_count() + i32::try_from(n).unwrap());
        }
        dropped
    }

    /// What the reader is looking at: the middle of the list, without
    /// the scrollbar, the header and whatever floats over the bottom.
    fn view(&mut self) -> Vec<(u8, u8, u8)> {
        self.frame();
        let mut out = Vec::with_capacity(340 * 260);
        for y in 140..400 {
            for x in 20..360 {
                let p = self.buf[y * 400 + x];
                out.push((p.red, p.green, p.blue));
            }
        }
        out
    }

    /// The reader's view did not move since `before`.
    fn report_still(&mut self, step: &str, before: &[(u8, u8, u8)]) {
        let now = self.view();
        let moved = now.iter().zip(before).filter(|(a, b)| a != b).count();
        let ok = moved == 0;
        println!(
            "{} {step:<58} {} of {} pixels changed",
            if ok { "ok  " } else { "FAIL" },
            moved,
            now.len()
        );
        if !ok {
            self.failures.push(step.to_string());
        }
    }

    /// What the host does: push rows, ask the pane, count if scrolled up.
    fn append(&mut self, n: usize, tall_every: usize) {
        for k in 0..n {
            let tall = tall_every > 0 && k % tall_every == tall_every - 1;
            let r = self.row(tall, false);
            self.model.push(r);
        }
        let following = self.ui.invoke_appended();
        if !following {
            let s = self.ui.global::<State>();
            s.set_chat_new_count(s.get_chat_new_count() + i32::try_from(n).unwrap());
        }
    }
    fn press(&mut self, x: f32, y: f32) {
        let position = LogicalPosition::new(x, y);
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
        self.frame();
    }
    fn drag_to(&mut self, x: f32, y: f32) {
        self.ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x, y),
        });
        self.frame();
    }
    fn release(&mut self, x: f32, y: f32) {
        let position = LogicalPosition::new(x, y);
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        self.ui.window().dispatch_event(WindowEvent::PointerExited);
        self.frame();
    }
    /// A history page put in front, the way the host does it.
    fn prepend(&mut self, n: usize, tall_every: usize) {
        for i in 0..n {
            let tall = tall_every > 0 && i % tall_every == tall_every - 1;
            let r = self.row(tall, false);
            self.model.insert(i, r);
        }
    }
    fn wheel(&mut self, notches: i32) {
        self.wheel_at(150.0, notches);
    }
    fn wheel_at(&mut self, x: f32, notches: i32) {
        // + is up (older), as the OS sends it.
        for _ in 0..notches.unsigned_abs() {
            let dy = if notches > 0 { 120.0 } else { -120.0 };
            self.ui
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(x, 300.0),
                    delta_x: 0.0,
                    delta_y: dy,
                });
            self.frames(3);
        }
    }
    fn state(&self) -> (bool, f32, i32) {
        (
            self.ui.invoke_following(),
            self.ui.invoke_gap(),
            self.ui.global::<State>().get_chat_new_count(),
        )
    }
    fn report(&mut self, step: &str, want_following: bool, want_at_bottom: Option<bool>) {
        let (f, g, c) = self.state();
        let at_bottom = g.abs() <= 2.0;
        let mut ok = f == want_following;
        if let Some(b) = want_at_bottom {
            ok &= at_bottom == b;
        }
        println!(
            "{} {step:<58} following={f:<5} gap={g:>8.1} unseen={c}",
            if ok { "ok  " } else { "FAIL" }
        );
        if !ok {
            self.failures.push(step.to_string());
        }
    }
}

fn main() {
    let win = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    win.set_size(slint::PhysicalSize::new(400, 640));
    slint::platform::set_platform(Box::new(P {
        window: win.clone(),
    }))
    .unwrap();
    let ui = TestWindow::new().unwrap();
    ui.show().unwrap();
    let model = Rc::new(VecModel::<ChatRow>::default());
    ui.global::<State>().set_chat(ModelRc::from(model.clone()));
    let mut sim = Sim {
        win,
        ui,
        model,
        buf: vec![PremultipliedRgbaColor::default(); 400 * 640],
        next: 0,
        held: false,
        failures: Vec::new(),
    };

    // A conversation opens: 150 rows, a picture every 6th, pinned.
    sim.append(150, 6);
    sim.ui.invoke_pin();
    sim.frames(60);
    sim.report("open: 150 rows, pinned", true, Some(true));

    if let Ok(dir) = std::env::var("SHOT") {
        let tr = sim.ui.global::<Tr>();
        tr.set_dm_general("General".into());
        tr.set_activity_show("Activity".into());
        sim.ui
            .global::<State>()
            .set_users_count_label("7 online".into());
        sim.wheel(5);
        sim.frames(30);
        sim.append(3, 0);
        let n = sim.ui.global::<State>().get_chat_new_count();
        sim.ui
            .global::<State>()
            .set_chat_new_label(format!("{n} new messages").into());
        sim.frames(40);
        sim.report("shot: scrolled up, 3 unseen", false, Some(false));
        sim.shot(&format!("{dir}/pill.ppm"));
        sim.ui.invoke_jump();
        sim.frames(90);
        sim.shot(&format!("{dir}/bottom.ppm"));
        return;
    }
    // Messages arrive while at the bottom (pictures among them).
    for round in 0..5 {
        sim.append(3, 2);
        sim.frames(40);
        sim.report(
            &format!("at bottom, 3 arrive (round {round})"),
            true,
            Some(true),
        );
    }

    // One wheel notch up must let go, and nothing pulls back down.
    sim.wheel(1);
    sim.frames(30);
    sim.report("one notch up", false, Some(false));
    let (_, gap_before, _) = sim.state();
    sim.append(4, 2);
    sim.frames(40);
    sim.report("scrolled up, 4 arrive: stays, counts 4", false, Some(false));
    let (_, gap_after, unseen) = sim.state();
    if gap_after <= gap_before || unseen != 4 {
        println!("FAIL view moved or miscounted ({gap_before} -> {gap_after}, {unseen})");
        sim.failures.push("count/position while scrolled up".into());
    }

    // Wheel back down to the end: holds again, zero unseen.
    sim.wheel(-12);
    sim.frames(30);
    sim.report("wheel back down to the end", true, Some(true));
    sim.append(2, 1);
    sim.frames(40);
    sim.report("then 2 pictures arrive: followed", true, Some(true));

    // Far up, then the jump button; then arrivals must be followed.
    sim.wheel(25);
    sim.frames(30);
    sim.report("25 notches up", false, Some(false));
    sim.append(3, 0);
    sim.frames(20);
    sim.ui.invoke_jump();
    sim.frames(90);
    sim.report("jump button", true, Some(true));
    for round in 0..6 {
        sim.append(2, 2);
        sim.frames(40);
        sim.report(
            &format!("after jump, 2 arrive (round {round})"),
            true,
            Some(true),
        );
    }

    // Same from the very top.
    sim.wheel(200);
    sim.frames(40);
    sim.report("all the way to the top", false, Some(false));
    sim.ui.invoke_jump();
    sim.frames(90);
    sim.report("jump from the top", true, Some(true));
    for round in 0..4 {
        sim.append(1, 1);
        sim.frames(40);
        sim.report(
            &format!("after jump from top, a picture (round {round})"),
            true,
            Some(true),
        );
    }

    // Small upward scrolls right after arrivals are never overridden.
    for round in 0..4 {
        sim.append(2, 2);
        sim.frames(2);
        sim.wheel(1);
        sim.frames(30);
        sim.report(
            &format!("notch up mid-follow (round {round})"),
            false,
            Some(false),
        );
        sim.ui.invoke_jump();
        sim.frames(90);
    }

    // ── Scrollbar drag: up lets go, arrivals during it do not pull ──
    sim.ui.invoke_jump();
    sim.frames(90);
    sim.report("before the scrollbar drag", true, Some(true));
    sim.press(383.0, 300.0);
    for k in 1..=10 {
        sim.drag_to(383.0, 300.0 - 4.0 * k as f32);
    }
    sim.append(2, 2);
    sim.frames(20);
    sim.report(
        "scrollbar dragged up, 2 arrive mid-drag",
        false,
        Some(false),
    );
    sim.release(383.0, 260.0);
    sim.frames(30);
    sim.report("scrollbar released", false, Some(false));

    // ── Touch: drag the content down (older), fling, arrivals ────────
    sim.ui.global::<State>().set_touch_host(true);
    sim.ui.invoke_jump();
    sim.frames(90);
    sim.report("touch host, at the bottom", true, Some(true));
    sim.press(150.0, 250.0);
    for k in 1..=8 {
        sim.drag_to(150.0, 250.0 + 15.0 * k as f32);
    }
    sim.release(150.0, 370.0);
    sim.append(2, 2);
    sim.frames(90);
    sim.report("finger dragged up and flung, 2 arrive", false, Some(false));
    sim.ui.invoke_jump();
    sim.frames(90);
    sim.report("touch: jump", true, Some(true));
    sim.ui.global::<State>().set_touch_host(false);

    // ── A picture decoded in place on the last row ───────────────────
    let last = sim.model.row_count() - 1;
    let mut r = sim.model.row_data(last).unwrap();
    r.kind = 2;
    r.img_w = 400.0;
    r.img_h = 420.0;
    sim.model.set_row_data(last, r);
    sim.frames(60);
    sim.report("last row grows into a picture: followed", true, Some(true));

    // ── The keyboard opens (the window gets shorter), and closes ─────
    sim.win.set_size(slint::PhysicalSize::new(400, 380));
    sim.frames(40);
    sim.report("window shorter (keyboard up)", true, Some(true));
    sim.win.set_size(slint::PhysicalSize::new(400, 640));
    sim.frames(40);
    sim.report("window back", true, Some(true));

    // ── Load earlier at the top: the view stays where it is ──────────
    sim.wheel(400);
    sim.frames(40);
    sim.report("at the top for Load earlier", false, Some(false));
    sim.prepend(50, 6);
    sim.frames(60);
    sim.report(
        "page of 50 put in front: no jump to the bottom",
        false,
        Some(false),
    );

    // ── A DM opens: empty, pinned, then its first page arrives ───────
    let dm = Rc::new(VecModel::<ChatRow>::default());
    sim.ui.global::<State>().set_chat(ModelRc::from(dm.clone()));
    sim.model = dm;
    sim.ui.invoke_pin();
    sim.frames(10);
    sim.frames(30);
    sim.prepend(50, 5);
    sim.frames(60);
    sim.report("DM: first page lands after the pin", true, Some(true));
    sim.append(2, 1);
    sim.frames(40);
    sim.report("DM: then 2 pictures arrive", true, Some(true));

    // ── The wheel over the scrollbar strip (no `scrolled` from it) ───
    sim.ui.invoke_jump();
    sim.frames(90);
    sim.report("before the wheel over the scrollbar", true, Some(true));
    sim.wheel_at(383.0, 2);
    sim.frames(30);
    sim.report("2 notches up over the scrollbar", false, Some(false));
    sim.append(2, 2);
    sim.frames(40);
    sim.report("then 2 arrive: not pulled down", false, Some(false));
    sim.wheel_at(383.0, -40);
    sim.frames(30);
    sim.report("down to the end over the scrollbar", true, Some(true));

    // ── Drag the thumb back down past the end: holds, then follows ───
    sim.wheel(30);
    sim.frames(30);
    sim.report("up again before the thumb drag down", false, Some(false));
    sim.press(383.0, 200.0);
    for k in 1..=12 {
        sim.drag_to(383.0, 200.0 + 60.0 * k as f32);
    }
    sim.frames(10);
    sim.report("thumb dragged past the end, still held", true, Some(true));
    sim.release(383.0, 920.0);
    sim.frames(30);
    sim.append(2, 2);
    sim.frames(40);
    sim.report("released at the end, 2 arrive: followed", true, Some(true));

    // ── The cap (review P3-43) ──────────────────────────────────────
    // The host keeps 500 rows: an append takes the oldest out from the
    // front. A reader at the newest rows must stay there through it.
    sim.ui.invoke_jump();
    sim.frames(60);
    sim.ui.global::<State>().set_chat_new_count(0);
    while sim.model.row_count() < MAX_ROWS {
        sim.append_capped(25, 6);
        sim.frames(12);
    }
    sim.frames(40);
    sim.report("filled to the cap at the bottom", true, Some(true));
    for round in 0..5 {
        let dropped = sim.append_capped(3, 2);
        sim.frames(40);
        sim.report(
            &format!("at the cap, 3 arrive and {dropped} leave (round {round})"),
            true,
            Some(true),
        );
    }

    // Scrolled up at the cap. The host learns that the reader left the
    // newest rows from the append that follows — so that one still
    // trims the front, above what they are reading; from the next on
    // the front is held. Neither may move what is on screen.
    sim.wheel(6);
    sim.frames(40);
    sim.report("scrolled up at the cap", false, Some(false));
    let before = sim.view();
    let dropped = sim.append_capped(4, 0);
    sim.frames(40);
    sim.report_still(
        &format!("the first append after it: {dropped} leave above the reader"),
        &before,
    );
    let before = sim.view();
    let dropped = sim.append_capped(4, 0);
    sim.frames(40);
    sim.report_still(
        &format!("the next one: the front is held, {dropped} leave"),
        &before,
    );

    // Held, the list grows to four times the cap, and then the oldest
    // rows go again — still above a reader who must not notice.
    while sim.model.row_count() < HELD_MAX_ROWS {
        let room = HELD_MAX_ROWS - sim.model.row_count();
        sim.append_capped(room.min(100), 6);
        sim.frames(4);
    }
    sim.frames(40);
    sim.report("the held list at four times the cap", false, Some(false));
    let before = sim.view();
    let dropped = sim.append_capped(40, 0);
    sim.frames(40);
    sim.report_still(
        &format!("past the held cap: {dropped} leave above the reader"),
        &before,
    );
    if dropped != 40 {
        println!("FAIL the held cap let {dropped} rows go, not 40");
        sim.failures.push("the held cap".into());
    }

    // Back at the newest rows. The host hears of it at the next append
    // (which still holds the front), and lets go at the one after:
    // that one trims three quarters of the list in one step.
    sim.ui.invoke_jump();
    sim.frames(90);
    sim.report("jump from the held list", true, Some(true));
    let dropped = sim.append_capped(2, 0);
    sim.frames(60);
    sim.report(&format!("then 2 arrive, {dropped} leave"), true, Some(true));
    let dropped = sim.append_capped(2, 0);
    sim.frames(60);
    sim.report(
        &format!("and 2 more: {dropped} leave at once"),
        true,
        Some(true),
    );
    for round in 0..3 {
        sim.append_capped(3, 2);
        sim.frames(40);
        sim.report(
            &format!("after the big trim, 3 arrive (round {round})"),
            true,
            Some(true),
        );
    }
    if sim.model.row_count() != MAX_ROWS {
        println!("FAIL the list holds {} rows", sim.model.row_count());
        sim.failures.push("the cap after the hold".into());
    }

    println!(
        "\n{} of the checks failed{}",
        sim.failures.len(),
        if sim.failures.is_empty() { "" } else { ":" }
    );
    for f in &sim.failures {
        println!("  {f}");
    }
    if !sim.failures.is_empty() {
        std::process::exit(1);
    }
}
