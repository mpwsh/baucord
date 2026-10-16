//! Audio device changes and warnings, settings that apply live (defaults, language, keybinds), showing the window.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    /// Called by the 2.5 s polling timer. All diff logic lives in
    /// app-core; this applies the side effects.
    pub fn handle_devices_changed(
        &mut self,
        new_input: Vec<(String, bool)>,
        new_output: Vec<(String, bool)>,
    ) {
        use app_core::devices::{self, DeviceNotice};

        // With nothing picked the engine follows the default by itself
        // (the watcher on Windows, its own poll elsewhere): this only
        // says so. It also told the engine to reopen on the default, a
        // second reopen after every change, and on Windows it compared
        // the console default while voice followed the communications
        // one (review 2026-10, P2-9). A picked device that went away
        // used to be erased from Settings; now the default stands in
        // until it is back, and the pick is kept.
        // Every poll, not only a changed one: a pick missing at start
        // has its stand-in from the first.
        let present =
            |devices: &[(String, bool)], name: &str| devices.iter().any(|(n, _)| n == name);
        // An empty list is a listing that failed, as often as not: no
        // stand-in on its word.
        if !new_input.is_empty()
            && let Some(pick) = self.settings.audio.input.device.clone()
        {
            let here = present(&new_input, &pick);
            if here == self.input_standin {
                self.input_standin = !here;
                self.voice
                    .tx
                    .send(VoiceControl::SetInputDevice(here.then_some(pick)))
                    .ok();
            }
        }
        // A phone's outputs are routes the engine follows by itself
        // (audio::route): a picked headset that went away is stood in
        // for by Automatic there.
        if !SINGLE_WINDOW_HOST
            && !new_output.is_empty()
            && let Some(pick) = self.settings.audio.output.device.clone()
        {
            let here = present(&new_output, &pick);
            if here == self.output_standin {
                self.output_standin = !here;
                self.voice
                    .tx
                    .send(VoiceControl::SetOutputDevice(here.then_some(pick)))
                    .ok();
                self.sync_stream_output();
            }
        }
        let diff = devices::diff(
            &self.last_input_devices,
            &new_input,
            &self.last_output_devices,
            &new_output,
            self.settings.audio.input.device.as_deref(),
            self.settings.audio.output.device.as_deref(),
        );
        if diff.unchanged {
            return;
        }

        for notice in &diff.notices {
            let text = match notice {
                DeviceNotice::MicConnected(n) => t!("chat-mic-connected", "name" => n.as_str()),
                DeviceNotice::MicDisconnected(n) => {
                    t!("chat-mic-disconnected", "name" => n.as_str())
                }
                DeviceNotice::SpeakerConnected(n) => {
                    t!("chat-speaker-connected", "name" => n.as_str())
                }
                DeviceNotice::SpeakerDisconnected(n) => {
                    t!("chat-speaker-disconnected", "name" => n.as_str())
                }
                DeviceNotice::InputLost => t!("chat-input-lost"),
                DeviceNotice::InputDefaultChanged(name) => {
                    let fallback = t!("unknown-device");
                    t!("chat-input-default-changed",
                        "name" => name.as_deref().unwrap_or(&fallback))
                }
                DeviceNotice::OutputLost => t!("chat-output-lost"),
                DeviceNotice::OutputDefaultChanged(name) => {
                    let fallback = t!("unknown-device");
                    t!("chat-output-default-changed",
                        "name" => name.as_deref().unwrap_or(&fallback))
                }
            };
            self.push_activity(&text);
        }

        // The share's sound is the system mix, not one device's; a
        // restart is cheap insurance when the output it plays to moves.
        if diff.output_moved && self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::RestartSystemAudio).ok();
        }

        // The pickers in Settings show what is there now, with the
        // selection the lines above may have just moved.
        let names = |devices: &[(String, bool)]| devices.iter().map(|(n, _)| n.clone()).collect();
        crate::settingsui::refresh_devices(self, names(&new_input), names(&new_output));

        self.last_input_devices = new_input;
        self.last_output_devices = new_output;
        self.refresh_device_warning();
    }

    /// Compare the devices saved in settings against what's connected
    /// right now and surface the result on the connect view.
    pub fn refresh_device_warning(&self) {
        let (missing_in, missing_out) = app_core::devices::missing_saved(
            self.settings.audio.input.device.as_deref(),
            self.settings.audio.output.device.as_deref(),
            &self.last_input_devices,
            &self.last_output_devices,
        );
        let mut missing: Vec<String> = Vec::new();
        if let Some(name) = missing_in {
            missing.push(t!("device-missing-mic", "name" => name));
        }
        if let Some(name) = missing_out {
            missing.push(t!("device-missing-output", "name" => name));
        }
        let text = if missing.is_empty() {
            String::new()
        } else {
            let joiner = format!(" {} ", t!("list-and"));
            format!(
                "{}\n{}",
                t!("device-warning", "devices" => missing.join(&joiner)),
                t!("device-warning-hint"),
            )
        };
        self.main.global::<State>().set_device_warning(text.into());
    }

    pub fn set_input_device(&mut self, device: Option<String>) {
        self.settings.audio.input.device.clone_from(&device);
        self.input_standin = false;
        self.voice
            .tx
            .send(VoiceControl::SetInputDevice(device))
            .ok();
        self.refresh_device_warning();
    }

    pub fn set_output_device(&mut self, device: Option<String>) {
        self.settings.audio.output.device.clone_from(&device);
        self.output_standin = false;
        self.voice
            .tx
            .send(VoiceControl::SetOutputDevice(device))
            .ok();
        self.sync_stream_output();
        self.refresh_device_warning();
    }

    /// Stream audio plays on the voice output picked here, and moves
    /// with it (review P1-19): the screen thread's decoders read this.
    /// A phone's pick is a call route the decoders ignore.
    pub(super) fn sync_stream_output(&self) {
        self.screen
            .shared
            .stream_output
            .set(self.engine_output().as_deref());
    }

    /// The microphone the engine is told to use: the pick, or the
    /// default while the pick is unplugged (`input_standin`).
    pub(super) fn engine_input(&self) -> Option<String> {
        self.settings
            .audio
            .input
            .device
            .clone()
            .filter(|_| !self.input_standin)
    }

    /// The output, likewise.
    pub(super) fn engine_output(&self) -> Option<String> {
        self.settings
            .audio
            .output
            .device
            .clone()
            .filter(|_| !self.output_standin)
    }

    /// The configuration a new engine starts with: the stand-ins
    /// applied.
    pub(super) fn engine_config(&self) -> audio::VoiceChatConfig {
        let mut config = client::voice::engine_config(&self.settings);
        config.input_device = self.engine_input();
        config.output_device = self.engine_output();
        config
    }

    pub fn voice_tx(&self) -> &crossbeam_channel::Sender<VoiceControl> {
        &self.voice.tx
    }

    /// The voice engine's meters, for the Settings meter loop.
    pub fn voice_levels(&self) -> client::voice::SharedAudioState {
        self.voice.shared.clone()
    }

    /// Once per install: a user whose other apps Windows is lowering
    /// because of our call streams — Windows 10, which cannot be asked
    /// not to — is told that it is Windows, and where the way out is
    /// (AEC-SPEC.md E15). From the device poll; waits until the window
    /// is where the toast can be read.
    pub fn tell_lowering_once(&mut self) {
        if self.settings.audio.lowering_told || !self.main_on_screen() {
            return;
        }
        let lowering = self.voice.shared.aec.lock().is_ok_and(|report| {
            report
                .as_ref()
                .is_some_and(audio::AecReport::lowering_others)
        });
        if !lowering {
            return;
        }
        self.settings.audio.lowering_told = true;
        self.save_settings();
        let text = t!("aec-lowering-told");
        self.push_activity(&text);
        self.show_toast(ToastKind::Warning, &text);
    }

    /// What echo cancellation the voice session got (`None` = none
    /// running), as the voice thread last published it.
    pub fn voice_aec(&self) -> Arc<std::sync::Mutex<Option<audio::AecReport>>> {
        self.voice.shared.aec.clone()
    }

    pub fn apply_reset_defaults(&mut self) {
        let before = self.settings.clone();
        self.settings.reset();
        let _ = std::fs::remove_file(client::config::avatar_path());
        self.forget_avatar_blob();
        // The picture went with the file: the rows stop showing it.
        self.reload_own_avatar();
        self.profile_dirty = true;
        self.apply_settings_live(&before);
    }

    /// Push the settings the Settings page edits to the running app, so
    /// it behaves as they say at once: after Reset to defaults, and
    /// after the page's Close puts back what it changed
    /// (`settingsui::discard_and_close`). `before` is what the app ran
    /// with until now: only an audio setting that differs goes to the
    /// voice engine — every Close used to reopen the microphone and the
    /// output, a gap in the call — and the connect view keeps a verdict
    /// it shows (review 2026-10, P2-2).
    pub(crate) fn apply_settings_live(&mut self, before: &client::config::Settings) {
        self.apply_text_scale();
        self.push_ui_prefs();
        self.apply_push_to_talk();
        let (now, was) = (&self.settings.audio, &before.audio);
        let mut controls = Vec::new();
        if now.gate_threshold.to_bits() != was.gate_threshold.to_bits() {
            controls.push(VoiceControl::SetGateThreshold(now.gate_threshold));
        }
        if now.input.gain.to_bits() != was.input.gain.to_bits() {
            controls.push(VoiceControl::SetInputGain(now.input.gain));
        }
        if now.output.gain.to_bits() != was.output.gain.to_bits() {
            controls.push(VoiceControl::SetOutputGain(now.output.gain));
        }
        if now.input.device != was.input.device {
            controls.push(VoiceControl::SetInputDevice(now.input.device.clone()));
        }
        if now.output.device != was.output.device {
            controls.push(VoiceControl::SetOutputDevice(now.output.device.clone()));
        }
        if now.echo_cancellation != was.echo_cancellation {
            controls.push(VoiceControl::SetEchoCancellation(now.echo_cancellation));
        }
        for control in controls {
            self.voice.tx.send(control).ok();
        }
        self.sync_stream_output();
        self.refresh_hotkeys();
        super::apply_log_level(&self.settings);
        if self.settings.language != before.language {
            i18n::set_locale(self.settings.language.as_deref());
            self.push_tr();
            self.relabel_for_language();
        }
        self.main
            .global::<State>()
            .set_nickname(self.settings.user.nickname.as_str().into());
        self.push_connect_servers();
        // Our name's colour and style, in the roster and our own rows.
        self.update_user_list();
        self.refresh_own_rows();
        self.refresh_device_warning();
    }

    pub fn set_language(&mut self, lang: Option<String>) {
        self.settings.language = lang;
        i18n::set_locale(self.settings.language.as_deref());
        // Labels are properties — pushing new strings re-renders every
        // view live, chat history stays as-is. (The FLTK build had to
        // rebuild views for this.)
        self.push_tr();
        self.push_connect_fields();
        self.relabel_for_language();
    }

    /// What Rust words itself, outside `Tr`, in the new language: the
    /// admin channel's name, the roster (its headers and fallbacks) and
    /// the tray's menu. They stayed in the old one until something else
    /// redrew them (review 2026-10, P3-1).
    pub(crate) fn relabel_for_language(&mut self) {
        if let Some(meta) = self.session.chat.dm_meta.get_mut(&ADMIN_CHANNEL_KEY) {
            meta.name = t!("admin-channel");
        }
        self.rebuild_dms_model();
        self.update_user_list();
        self.sync_tray();
        crate::settingsui::relabel_licenses(self);
    }

    pub fn set_keybind(&mut self, action: BindableAction, bind: String) {
        self.settings.set_bind(action, bind);
        self.refresh_hotkeys();
    }

    /// The keybinds or push-to-talk changed: the push-to-talk key is
    /// watched only while push-to-talk is on (review P1-3).
    pub fn refresh_hotkeys(&mut self) {
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.update(&self.settings);
        }
    }

    /// The permission to watch keys was just granted: bindings that
    /// were registered as hotkeys meanwhile are watched from here on.
    pub fn rearm_hotkeys(&mut self) {
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.rearm();
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  CHAT MODEL
    // ════════════════════════════════════════════════════════════════

    /// Show the main window, arming the reveal hook so the full
    /// repaint lands on the first OS paint of the (possibly fresh)
    /// surface — see [`repaint_on_reveal`]. Also for a window that is
    /// merely behind others or minimized (the tray's "Show", a second
    /// launch of the binary): `show()` alone would leave it there.
    pub fn show_main(&self) {
        self.set_app_shown(true);
        self.main_repaint_arm.set(true);
        let _ = self.main.show();
        self.place_main_once();
        raise_window(self.main.window());
    }

    /// Centre the main window on its monitor the first time it is
    /// shown (a launch, or the tray's first "Show" after "start in the
    /// tray"). Never again afterwards: a user who moved it keeps it
    /// there for the rest of the run.
    pub(super) fn place_main_once(&self) {
        if self.main_placed.replace(true) {
            return;
        }
        center_on_monitor(self.main.window());
    }

    // NOTE: there is deliberately no scroll-after-append here. The
    // ListView pins itself to the bottom reactively (chat.slint's
    // stick-to-bottom `changed` handlers), which runs AFTER layout —
    // a host-side invoke right after a model push reads a stale
    // content-height and lands short (or permanently unsticks once an
    // image row overshoots the at-bottom slack).
}
