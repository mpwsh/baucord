//! Latency and stream stats, memory and resource sampling, toasts, the update flags, saving settings and geometry, shutdown.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    pub(super) fn update_latency(
        &self,
        rtt_ms: f64,
        loss_pct: f64,
        audio_latency_ms: Option<u32>,
        kbps_up: f64,
        kbps_down: f64,
    ) {
        use app_core::stats::{latency_grade, net_str, LatencyGrade};
        let s = self.main.global::<State>();
        s.set_status_text(t!("status-connected").into());
        s.set_status_color(match latency_grade(rtt_ms) {
            LatencyGrade::Good => slint::Color::from_rgb_u8(0x2B, 0xAC, 0x76),
            LatencyGrade::Fair => slint::Color::from_rgb_u8(0xEC, 0xB2, 0x2E),
            LatencyGrade::Bad => slint::Color::from_rgb_u8(0xE0, 0x1E, 0x5A),
        });
        let audio_str = match audio_latency_ms {
            Some(ms) => format!("{ms}ms"),
            None => "--".to_string(),
        };
        s.set_latency_line(
            t!("latency-line",
                "audio" => audio_str,
                "network" => net_str(rtt_ms, loss_pct, |rtt, loss| t!("latency-net-loss",
                    "rtt" => rtt, "loss" => loss)))
            .into(),
        );
        // In Mb, beside the arrows the sidebar draws.
        s.set_latency_up(format!("{:.1}", kbps_up / 1000.0).into());
        s.set_latency_down(format!("{:.1}", kbps_down / 1000.0).into());
    }

    /// The share is over: stop its timer and clear the stats line (it
    /// kept showing the last sample).
    pub(super) fn stop_stream_stats(&mut self) {
        self.stream_stats_timer.stop();
        self.stream_viewers_seen = 0;
        self.main.global::<State>().set_stream_stats("".into());
    }

    pub(super) fn update_stream_stats(&mut self) {
        if !self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            // The share ended without its event reaching us.
            self.stop_stream_stats();
            return;
        }
        let sh = &self.screen.shared;
        let frame_bytes = sh.last_frame_bytes.load(Ordering::Relaxed);
        let encode_us = sh.last_encode_us.load(Ordering::Relaxed);
        let viewers = sh.stream_viewers.load(Ordering::Relaxed);
        // Someone new is watching (the count only reaches us through
        // the screen thread's health sample, once a second).
        if viewers > self.stream_viewers_seen && self.settings.sounds.viewer_joined_on() {
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::ViewerJoined))
                .ok();
        }
        self.stream_viewers_seen = viewers;
        let fps_scale_pct = sh.fps_scale_pct.load(Ordering::Relaxed);
        let loss_pct = f32::from_bits(sh.stream_loss_pct.load(Ordering::Relaxed));
        let quality_str = sh
            .current_quality
            .lock()
            .ok()
            .and_then(|q| *q)
            .map_or_else(|| t!("unknown"), |q| q.to_string());
        if !self.main_on_screen() {
            return;
        }
        let line1 = t!("stream-stats-quality",
            "quality" => quality_str.as_str(), "viewers" => viewers);
        let mut line2 = t!("stream-stats-frame",
            "kb" => format!("{:.0}", frame_bytes as f32 / 1024.0),
            "ms" => format!("{:.1}", encode_us as f32 / 1000.0));
        // Below 100 the sender is shedding fps for lossy viewers: the
        // block turns warning-colored, so the sharer notices without
        // reading numbers.
        let degraded = fps_scale_pct < 100;
        if degraded {
            line2.push_str(" · ");
            line2.push_str(&t!("stream-stats-degraded",
                "fps" => fps_scale_pct,
                "loss" => format!("{loss_pct:.1}")));
        }
        let s = self.main.global::<State>();
        s.set_stream_stats(format!("{line1}\n{line2}").into());
        s.set_stream_degraded(degraded);
    }

    /// Whether the main screen is actually being rendered: false on the
    /// connect view, under a page (Settings, the admin panel, a phone's
    /// stream — main.slint's `main-shown`), hidden to the tray,
    /// minimized, or with a phone app in the background. Stats updates,
    /// resource sampling, roster rebuilds and chat GIFs wait while this
    /// is false (review 2026-10, P2-30); `Logic.main-screen-shown`
    /// catches them up.
    pub(super) fn main_on_screen(&self) -> bool {
        let s = self.main.global::<State>();
        // A phone's picture viewer covers it whole.
        let viewer_covers = s.get_viewer_open() && s.get_touch_host();
        s.get_phase() == Phase::Main
            && !s.get_settings_open()
            && !s.get_admin_open()
            && !s.get_watch_open()
            && !viewer_covers
            && host_visible()
            && self.main.window().is_visible()
            && !self.main.window().is_minimized()
    }

    /// A phone app is back on screen (`set_host_visible`): restart what
    /// the background paused. The stats lines come back with the next
    /// 2-second sample by themselves.
    pub(super) fn host_shown(&mut self) {
        self.set_app_shown(true);
        // The stream left in the background: asked for again; the
        // server starts it with a keyframe.
        if let Some(id) = self.paused_watch.take()
            && self.viewers.contains_key(&id)
            && self.session.active_sharers.contains_key(&id)
        {
            self.screen
                .tx
                .send(ScreenControl::Watch { client_id: id })
                .ok();
            self.network
                .tx
                .send(NetworkControl::Send(ClientMessage::SubscribeScreen {
                    target_id: id,
                }))
                .ok();
        }
        self.clear_seen_notices();
        // The roster changes the background held back (review P1-9).
        self.flush_roster();
        // Android: a room entered while off screen could not have its
        // microphone type (a background start); on screen it can.
        if cfg!(target_os = "android") {
            self.sync_keep_alive(true);
        }
        self.resume_gifs();
        // Settings' Audio tab, if that is what comes back: its meter
        // and the microphone behind it, without waiting for the
        // settings timer's slow tick.
        crate::settingsui::sync_audio_tab(self);
    }

    /// A phone app left the screen (`set_host_visible`). The Audio
    /// tab's microphone goes with it, at once: out of a room nothing
    /// may record behind the user's back, and an open microphone would
    /// keep iOS from suspending an app that is not even connected.
    pub(super) fn host_hidden(&mut self) {
        self.set_app_shown(false);
        // A phone in the background stops watching: nothing of the
        // stream comes any more, its sound included — about 3.6 GB an
        // hour came down for a page nobody saw (review 2026-10, P1-2;
        // ANDROID-SPEC.md D74). The page stays, and asks again on
        // return.
        if let Some(id) = self.page_viewer_id() {
            self.paused_watch = Some(id);
            self.screen
                .tx
                .send(ScreenControl::StopWatching { client_id: id })
                .ok();
            self.network
                .tx
                .send(NetworkControl::UnsubscribeScreen { target_id: id })
                .ok();
        }
        self.set_audio_tab_open(false);
    }

    /// Whether anything of ours is on screen, for what only draws
    /// (`State.app-shown`: the chat's follow). Off in the tray,
    /// minimized, wholly covered, or a phone app in the background.
    pub(crate) fn set_app_shown(&self, shown: bool) {
        let s = self.main.global::<State>();
        if s.get_app_shown() == shown {
            return;
        }
        s.set_app_shown(shown);
        // The screen's own timers at the pace of what is seen: shown,
        // the device poll every 2.5 s and the resource line every 2 s;
        // out of sight the poll every 10 s (a picked device that went
        // away still falls back to the default) and no resource line.
        // Both ran at full pace from launch to exit (review 2026-10,
        // P2-33).
        self.device_timer
            .set_interval(Duration::from_millis(if shown { 2500 } else { 10_000 }));
        if shown {
            self.resource_timer.restart();
        } else {
            self.resource_timer.stop();
        }
        // The link stats feed only the screen: the network thread stops
        // handing them over (P2-48, already so on the phones).
        client::network::set_ui_visible(shown);
    }

    /// The desktop's main window is back in view (out of the tray,
    /// restored, uncovered): what waited for that happens now.
    pub(crate) fn main_revealed(&mut self) {
        self.set_app_shown(true);
        // Settings, if that is what is back: its timer and meters.
        crate::settingsui::sync_audio_tab(self);
        // A roster held back while out of sight (review P1-9).
        self.flush_roster();
        self.resume_gifs();
    }

    /// `/mem`: where the client's memory goes, from what it holds. The
    /// process figure the task monitor shows, beside every pixel
    /// buffer and row the app retains — so a "the RAM tripled" report
    /// becomes a number per bucket instead of a guess. Texture bytes
    /// are `width × height × 4` of each `slint::Image` held; the
    /// renderer's own caches (Skia's glyphs and surfaces) and the
    /// allocator's slack are the remainder.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn memory_report(&mut self) -> String {
        let mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        let texture = |img: &slint::Image| {
            let size = img.size();
            size.width as usize * size.height as usize * 4
        };
        let rss = app_core::usage::memory()
            .map_or_else(|| "?".to_string(), |b| format!("{:.0} MB", mb(b as usize)));

        // Chat rows over every conversation; their pictures are in the
        // store (wire bytes, and previews of those recently on screen)
        // but for a playing GIF's frame, which its row holds.
        let parked_rows: usize = self
            .session
            .chat
            .parked
            .values()
            .map(|conv| conv.model.row_count())
            .sum();
        let rows = self.chat_model.row_count() + parked_rows;
        let frame_bytes: usize = self
            .chat_model
            .iter()
            .filter(|row| row.kind == 2)
            .map(|row| texture(&row.image))
            .sum();
        let (picture_count, compressed, preview_count, preview_bytes) = crate::pictures::stats();
        let preview_bytes = preview_bytes + frame_bytes;
        let gifs_playing = self.gifs.values().filter(|g| g.playing()).count();
        let avatars: usize = self.avatar_images.values().map(texture).sum();
        let own_avatar = self.own_avatar.as_ref().map_or(0, texture);
        let viewers = self.viewers.len();

        format!(
            "Memory — process {rss}\n\
             chat: {rows} rows ({parked_rows} parked in {} conversations), \
             {picture_count} pictures: {preview_count} decoded, {:.1} MB, \
             {:.1} MB compressed, \
             {} GIF players ({gifs_playing} playing)\n\
             avatars: {} decoded, {:.1} MB (+{:.1} MB ours)\n\
             roster: {} users, {} profiles; {viewers} stream viewers open",
            self.session.chat.parked.len(),
            mb(preview_bytes),
            mb(compressed),
            self.gifs.len(),
            self.avatar_images.len(),
            mb(avatars),
            mb(own_avatar),
            self.session.users.len(),
            self.session.profiles.len(),
        )
    }

    #[allow(clippy::cast_precision_loss)]
    pub(super) fn sample_resources(&mut self) {
        // Sample only when someone can actually see the result. The
        // check comes first so a hidden/minimized app pays nothing —
        // not even the look at the process.
        if !self.main_on_screen() {
            return;
        }
        // % of one core (0 the first time, with nothing to measure
        // from); the memory as the user's own task monitor shows it.
        let cpu_one_core = self.usage.percent_of_one_core().unwrap_or(0.0);
        if let Some(mem_bytes) = app_core::usage::memory() {
            // Normalize to % of the whole machine (task-manager style).
            let cores =
                std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f32;
            let cpu_pct = cpu_one_core / cores;
            let mem_mb = mem_bytes as f64 / (1024.0 * 1024.0);
            self.main.global::<State>().set_resource_line(
                t!("resource-line",
                    "cpu" => format!("{cpu_pct:.1}"),
                    "mem" => format!("{mem_mb:.0}"))
                .into(),
            );
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  MISC
    // ════════════════════════════════════════════════════════════════

    /// Write the settings file — without what the open Settings page
    /// changed: only its Save and close keeps that (ANDROID-SPEC.md
    /// D73). Every save in the app goes through here.
    pub(crate) fn save_settings(&self) {
        crate::settingsui::settings_to_write(self).save();
    }

    /// Persist geometry + settings immediately (a quit, and a host
    /// going into the background — the normal shutdown also does this,
    /// but only after the loop exits).
    pub fn save_settings_now(&mut self) {
        self.save_window_geometry();
        self.save_settings();
    }

    /// Leave the app as `how` says, unless the Settings page holds
    /// changes nobody saved: then the window comes up and asks first —
    /// Save and quit, Quit without saving, or Cancel (`answer_leave`).
    /// Every quit the user starts comes here; when the OS ends the app
    /// there is nobody to ask, and `shutdown` keeps nothing unsaved.
    pub(crate) fn leave_app(&mut self, how: Leaving) {
        if crate::settingsui::has_unsaved(self) {
            self.pending_leave = Some(how);
            self.show_main();
            self.main.global::<State>().set_quit_ask_open(true);
            return;
        }
        self.finish_leaving(how);
    }

    /// The question's answer: keep the page's changes (`Some(true)`),
    /// drop them (`Some(false)`), or stay (`None`).
    pub(crate) fn answer_leave(&mut self, keep: Option<bool>) {
        self.main.global::<State>().set_quit_ask_open(false);
        let Some(how) = self.pending_leave.take() else {
            return;
        };
        match keep {
            Some(true) => crate::settingsui::save_for_quit(self),
            Some(false) => crate::settingsui::drop_for_quit(self),
            None => {
                // The Settings page under the dialog takes the keyboard
                // back.
                self.main.invoke_focus_composer();
                return;
            }
        }
        self.finish_leaving(how);
    }

    fn finish_leaving(&mut self, how: Leaving) {
        self.save_settings_now();
        match how {
            Leaving::Quit => {
                let _ = slint::quit_event_loop();
            }
            Leaving::InstallUpdate(path) => {
                // Off the UI thread: it ends the process when it is done.
                let spawned = std::thread::Builder::new()
                    .name("update-install".into())
                    .spawn(move || {
                        if let Err(e) = crate::updater::extract_and_install(&path) {
                            let _ = slint::invoke_from_event_loop(move || {
                                with_current(|app| app.show_error(&e));
                            });
                        }
                    });
                if let Err(e) = spawned {
                    self.show_error(&e.to_string());
                }
            }
        }
    }

    pub fn show_error(&mut self, error: &str) {
        self.show_toast(ToastKind::Error, error);
    }

    /// A transient notice in the toast's register: info for what just
    /// happened, success for good news, warning for something to act
    /// on, error for what failed.
    pub fn show_toast(&mut self, kind: ToastKind, text: &str) {
        let s = self.main.global::<State>();
        s.set_toast_kind(kind);
        s.set_error_text(text.into());
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        if let Some(weak) = weak {
            self.error_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_secs(5),
                move || {
                    with_app(&weak, |app| {
                        app.main.global::<State>().set_error_text("".into());
                    });
                },
            );
        }
    }

    /// Update-flow state transitions (called from `updater.rs`).
    pub fn set_update_available(&mut self, remote_version: String) {
        let s = self.main.global::<State>();
        s.set_update_heading(t!("update-available-heading").into());
        s.set_update_body(
            t!("update-version-info",
                "current" => CURRENT_VERSION,
                "latest" => remote_version.as_str())
            .into(),
        );
        s.set_phase(Phase::UpdatePrompt);
        self.update_version = Some(remote_version);
    }

    pub fn update_version(&self) -> Option<&str> {
        self.update_version.as_deref()
    }

    pub fn download_cancel_flag(&self) -> Arc<AtomicBool> {
        self.download_cancelled.clone()
    }

    pub fn set_download_done(&mut self, path: String) {
        let s = self.main.global::<State>();
        s.set_update_heading(t!("update-downloaded-heading").into());
        s.set_update_body(t!("update-ready-info").into());
        s.set_update_progress(1.0);
        s.set_update_progress_text("".into());
        s.set_phase(Phase::DownloadDone);
        self.update_zip_path = Some(path);
    }

    pub fn set_download_failed(&mut self, error: &str) {
        let s = self.main.global::<State>();
        s.set_update_heading(t!("update-failed-heading").into());
        s.set_update_body(error.into());
        s.set_phase(Phase::DownloadFailed);
    }

    // Window px values fit comfortably in every one of these casts.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub(super) fn save_window_geometry(&mut self) {
        let size = self.main.window().size();
        let scale = self.main.window().scale_factor();
        self.settings.window.width = (size.width as f32 / scale) as i32;
        self.settings.window.height = (size.height as f32 / scale) as i32;
    }

    /// Push every localized static label into the Tr global (called at
    /// startup and on language change — this is live language
    /// switching, no rebuilds).
    #[allow(clippy::too_many_lines)]
    pub fn push_tr(&self) {
        // Settings and the admin panel are pages of the main window:
        // its Tr is theirs. (A stream viewer window fills its own when
        // it opens.)
        fill_tr(&self.main.global::<Tr>());
    }

    pub(super) fn push_connect_fields(&self) {
        let s = self.main.global::<State>();
        s.set_nickname(self.settings.user.nickname.as_str().into());
        s.set_invite_prompt(false);
        s.set_invite_prompt_text("".into());
        s.set_invite_code("".into());
        s.set_connect_notice("".into());
        self.push_connect_servers();
    }

    /// Refresh the connect view's server dropdown (names + which entry
    /// is selected). Called whenever the Servers tab mutates the list.
    pub fn push_connect_servers(&self) {
        let s = self.main.global::<State>();
        let names: Vec<slint::SharedString> = self
            .settings
            .servers
            .iter()
            .map(|e| e.name.as_str().into())
            .collect();
        let default_pos = self
            .settings
            .servers
            .iter()
            .position(|e| e.name == self.settings.server)
            .unwrap_or(0);
        s.set_server_names(ModelRc::new(VecModel::from(names)));
        s.set_server_index(i32::try_from(default_pos).unwrap_or(0));
    }

    /// Clean shutdown: signal threads, join with timeout, save settings.
    ///
    /// Order matters:
    /// 1. Save settings (while the window still exists for geometry)
    /// 2. Stop voice engine (drops audio devices, closes threads)
    /// 3. Disconnect network (sends goodbye to server)
    /// 4. Signal all worker threads to exit
    /// 5. Join with hard timeout — never hang forever
    pub(super) fn shutdown(&mut self) {
        info!("Shutting down");

        // Settings still open with changes: nobody chose to keep them.
        crate::settingsui::put_back_unsaved(self);
        self.save_window_geometry();
        self.save_settings();

        self.voice.tx.send(VoiceControl::Stop).ok();

        // No delay needed: the control channel is FIFO, so the network
        // thread flushes the disconnect packet before it ever sees the
        // Shutdown queued below — and step 5 joins the thread regardless.
        self.network.tx.send(NetworkControl::Disconnect).ok();

        self.voice.tx.send(VoiceControl::Shutdown).ok();
        self.network.tx.send(NetworkControl::Shutdown).ok();
        self.screen.tx.send(ScreenControl::Shutdown).ok();

        let voice_handle = self.voice.join_handle.take();
        let network_handle = self.network.join_handle.take();
        let screen_handle = self.screen.join_handle.take();

        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            if let Some(h) = voice_handle {
                let _ = h.join();
            }
            if let Some(h) = network_handle {
                let _ = h.join();
            }
            if let Some(h) = screen_handle {
                let _ = h.join();
            }
            let _ = done_tx.send(());
        });

        match done_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => info!("Shutdown complete"),
            Err(_) => warn!("Shutdown timed out — threads still running, exiting anyway"),
        }
    }
}
