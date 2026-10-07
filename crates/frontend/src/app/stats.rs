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
        s.set_status_text(format!("●  {}", t!("status-connected")).into());
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
                "network" => net_str(rtt_ms, loss_pct),
                "up" => format!("{:.1}", kbps_up / 1000.0),
                "down" => format!("{:.1}", kbps_down / 1000.0))
            .into(),
        );
    }

    #[allow(clippy::cast_precision_loss)]
    pub(super) fn update_stream_stats(&mut self) {
        if !self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.stream_viewers_seen = 0;
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
        let stats = app_core::stats::StreamStats {
            quality: quality_str,
            avg_frame_kb: frame_bytes as f32 / 1024.0,
            encode_ms: encode_us as f32 / 1000.0,
            viewers,
            fps_scale_pct,
            loss_pct,
        };
        let line1 = t!("stream-stats-quality",
            "quality" => stats.quality.as_str(), "viewers" => stats.viewers);
        let mut line2 = t!("stream-stats-frame",
            "kb" => format!("{:.0}", stats.avg_frame_kb),
            "ms" => format!("{:.1}", stats.encode_ms));
        if stats.degraded() {
            line2.push_str(" · ");
            line2.push_str(&t!("stream-stats-degraded",
                "fps" => stats.fps_scale_pct,
                "loss" => format!("{:.1}", stats.loss_pct)));
        }
        let s = self.main.global::<State>();
        s.set_stream_stats(format!("{line1}\n{line2}").into());
        s.set_stream_degraded(stats.degraded());
    }

    /// Whether the main screen is actually being rendered: false on the
    /// connect view, hidden to the tray, minimized, or with a phone app
    /// in the background. Stats updates and resource sampling are
    /// skipped while this is false.
    pub(super) fn main_on_screen(&self) -> bool {
        self.main.global::<State>().get_phase() == Phase::Main
            && host_visible()
            && self.main.window().is_visible()
            && !self.main.window().is_minimized()
    }

    /// A phone app is back on screen (`set_host_visible`): restart what
    /// the background paused. The stats lines come back with the next
    /// 2-second sample by themselves.
    pub(super) fn host_shown(&mut self) {
        self.clear_seen_notices();
        let animating = self.gallery_gif.is_some()
            || self.preview_gif.is_some()
            || self.gifs.values().any(GifPlayer::playing);
        if animating {
            self.ensure_gif_ticker();
        }
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
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
        let mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        let texture = |img: &slint::Image| {
            let size = img.size();
            size.width as usize * size.height as usize * 4
        };
        let rss = self.pid.and_then(|pid| {
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_memory(),
            );
            self.sys.process(pid).map(sysinfo::Process::memory)
        });
        let rss = crate::resources::native_monitor_memory()
            .or(rss)
            .map_or_else(|| "?".to_string(), |b| format!("{:.0} MB", mb(b as usize)));

        // Chat rows and their pictures, over every conversation.
        let mut rows = self.chat_model.row_count();
        let mut preview_bytes = 0usize;
        let mut preview_count = 0usize;
        for i in 0..self.chat_model.row_count() {
            if let Some(row) = self.chat_model.row_data(i)
                && row.kind == 2
            {
                preview_count += 1;
                preview_bytes += texture(&row.image);
            }
        }
        let mut parked_rows = 0usize;
        for conv in self.parked.values() {
            parked_rows += conv.model.row_count();
            for i in 0..conv.model.row_count() {
                if let Some(row) = conv.model.row_data(i)
                    && row.kind == 2
                {
                    preview_count += 1;
                    preview_bytes += texture(&row.image);
                }
            }
        }
        rows += parked_rows;
        let compressed: usize = self.images.values().map(|e| e.bytes.len()).sum();
        let gifs_playing = self.gifs.values().filter(|g| g.playing()).count();
        let avatars: usize = self.avatar_images.values().map(texture).sum();
        let own_avatar = self.own_avatar.as_ref().map_or(0, texture);
        let viewers = self.viewers.len();

        format!(
            "Memory — process {rss}\n\
             chat: {rows} rows ({parked_rows} parked in {} conversations), \
             {preview_count} pictures: {:.1} MB of previews, {:.1} MB compressed, \
             {} GIF players ({gifs_playing} playing)\n\
             avatars: {} decoded, {:.1} MB (+{:.1} MB ours)\n\
             roster: {} users, {} profiles; {viewers} stream viewers open",
            self.parked.len(),
            mb(preview_bytes),
            mb(compressed),
            self.gifs.len(),
            self.avatar_images.len(),
            mb(avatars),
            mb(own_avatar),
            self.users.len(),
            self.profiles.len(),
        )
    }

    #[allow(clippy::cast_precision_loss)]
    pub(super) fn sample_resources(&mut self) {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};

        // Sample only when someone can actually see the result. The
        // check comes first so a hidden/minimized app pays nothing —
        // not even the process-table refresh.
        let Some(pid) = self.pid else { return };
        if !self.main_on_screen() {
            return;
        }
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
        if let Some(proc) = self.sys.process(pid) {
            // cpu_usage() is % of one core; normalize to % of the whole
            // machine (task-manager style).
            let cores =
                std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f32;
            let cpu_pct = proc.cpu_usage() / cores;
            // Memory: the platform-native figure (what the user's own
            // task monitor shows), with sysinfo's RSS as the fallback.
            let mem_bytes =
                crate::resources::native_monitor_memory().unwrap_or_else(|| proc.memory());
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

    /// Persist geometry + settings immediately (the tray's Quit, and a
    /// host going into the background — the normal shutdown also does
    /// this, but only after the loop exits).
    pub fn save_settings_now(&mut self) {
        self.save_window_geometry();
        self.settings.save();
    }

    /// Keep a timer alive for the app's lifetime.
    pub fn hold_timer(&mut self, timer: slint::Timer) {
        self.timers.push(timer);
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
        fill_tr(&self.main.global::<Tr>());
        if let Some(win) = &self.settings_win {
            fill_tr(&win.global::<Tr>());
        }
        // The panel is created once and kept: it follows a language
        // change like the others.
        if let Some(win) = &self.admin_win {
            fill_tr(&win.global::<Tr>());
        }
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

        self.save_window_geometry();
        self.settings.save();

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
