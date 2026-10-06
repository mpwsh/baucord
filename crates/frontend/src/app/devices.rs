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
            self.push_chat_system(&text);
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
            // WASAPI loopback captures from the default output device,
            // so when that device changes the capture stream dies too.
            // Restart it if a screen share is currently active.
            if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                self.screen.tx.send(ScreenControl::RestartSystemAudio).ok();
            }
        }

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
        self.refresh_device_warning();
    }

    pub fn voice_tx(&self) -> &crossbeam_channel::Sender<VoiceControl> {
        &self.voice.tx
    }

    pub fn voice_levels(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicU32>,
        Arc<std::sync::atomic::AtomicU32>,
    ) {
        (
            self.voice.shared.output_level.clone(),
            self.voice.shared.denoised_level.clone(),
        )
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
        self.apply_text_scale();
        self.push_ui_prefs();
        self.apply_push_to_talk();
        // Push the defaults to the running subsystems so behavior
        // matches the file immediately.
        self.voice
            .tx
            .send(VoiceControl::SetGateThreshold(
                self.settings.audio.gate_threshold,
            ))
            .ok();
        self.voice
            .tx
            .send(VoiceControl::SetInputGain(self.settings.audio.input.gain))
            .ok();
        self.voice
            .tx
            .send(VoiceControl::SetOutputGain(self.settings.audio.output.gain))
            .ok();
        self.voice.tx.send(VoiceControl::SetInputDevice(None)).ok();
        self.voice.tx.send(VoiceControl::SetOutputDevice(None)).ok();
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.update(&self.settings);
        }
        i18n::set_locale(self.settings.language.as_deref());
        self.push_tr();
        self.push_connect_fields();
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
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.update(&self.settings);
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
    // viewport-height and lands short (or permanently unsticks once an
    // image row overshoots the at-bottom slack).
}
