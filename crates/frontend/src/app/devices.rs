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

        if diff.rebuild_input {
            self.settings.audio.input.device = None;
            self.voice.tx.send(VoiceControl::SetInputDevice(None)).ok();
        }
        // A phone's outputs are routes the engine follows by itself
        // (audio::route): a picked headset that went away is stood in
        // for by Automatic until it is back, so the pick is kept.
        if diff.rebuild_output && !SINGLE_WINDOW_HOST {
            self.settings.audio.output.device = None;
            self.voice.tx.send(VoiceControl::SetOutputDevice(None)).ok();
            self.sync_stream_output();
            // WASAPI loopback captures from the default output device,
            // so when that device changes the capture stream dies too.
            // Restart it if a screen share is currently active.
            if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                self.screen.tx.send(ScreenControl::RestartSystemAudio).ok();
            }
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
        self.voice
            .tx
            .send(VoiceControl::SetInputDevice(device))
            .ok();
        self.refresh_device_warning();
    }

    pub fn set_output_device(&mut self, device: Option<String>) {
        self.settings.audio.output.device.clone_from(&device);
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
            .set(self.settings.audio.output.device.as_deref());
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
        self.settings.save();
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
        self.settings.reset();
        let _ = std::fs::remove_file(client::config::avatar_path());
        self.forget_avatar_blob();
        self.profile_dirty = true;
        self.apply_settings_live();
    }

    /// Push every setting the Settings page edits to the running app,
    /// so it behaves as the settings say at once: after Reset to
    /// defaults, and after the page's Close puts back what it changed
    /// (`settingsui::discard_and_close`).
    pub(crate) fn apply_settings_live(&mut self) {
        self.apply_text_scale();
        self.push_ui_prefs();
        self.apply_push_to_talk();
        let audio = &self.settings.audio;
        for control in [
            VoiceControl::SetGateThreshold(audio.gate_threshold),
            VoiceControl::SetInputGain(audio.input.gain),
            VoiceControl::SetOutputGain(audio.output.gain),
            VoiceControl::SetInputDevice(audio.input.device.clone()),
            VoiceControl::SetOutputDevice(audio.output.device.clone()),
            VoiceControl::SetEchoCancellation(audio.echo_cancellation),
        ] {
            self.voice.tx.send(control).ok();
        }
        self.sync_stream_output();
        self.refresh_hotkeys();
        super::apply_log_level(&self.settings);
        i18n::set_locale(self.settings.language.as_deref());
        self.push_tr();
        self.push_connect_fields();
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
