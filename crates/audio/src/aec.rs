//! Acoustic echo cancellation: the cross-platform contract
//! (`AEC-SPEC.md` §3).
//!
//! baucord carries no canceller of its own. Every OS ships one that
//! already has the hard part — the reference signal, i.e. exactly what
//! the speakers played and when — and switches it on for streams that
//! say they are a call. So the engine's whole job is: decide whether
//! this session wants the platform's voice-call path ([`wants_aec`]),
//! open both streams that way, and report what it actually got
//! ([`AecReport`]) instead of assuming.
//!
//! What "the voice-call path" is per platform lives with each backend:
//! Android — `VoiceCommunication` usage and input preset plus
//! `MODE_IN_COMMUNICATION` (`native::aaudio`); Windows — the
//! `Communications` stream category (`native::wasapi`). Everything in
//! this file is platform-free and unit-tested on the host.

use std::fmt;

/// The user's setting, `audio.echo_cancellation` in `settings.json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum AecMode {
    /// Only when the sound comes out of speakers (the default): nobody
    /// on headphones gets the call path's side effects (Windows ducking
    /// other apps, Android switching Bluetooth to the headset profile).
    #[default]
    Auto,
    /// Whatever the output is.
    Always,
    /// Never.
    Off,
}

impl AecMode {
    /// Every mode, in the order the settings picker shows them.
    pub const ALL: [Self; 3] = [Self::Auto, Self::Always, Self::Off];

    /// Position in [`Self::ALL`] (the picker index).
    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }

    /// The mode at picker index `i`; out of range → `Auto`.
    #[must_use]
    pub fn from_index(i: usize) -> Self {
        Self::ALL.get(i).copied().unwrap_or_default()
    }
}

/// What the sound comes out of, as far as the platform says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputKind {
    /// Built-in or external speakers: the microphone hears them.
    Speakers,
    /// Headphones or a headset: it does not (meaningfully).
    Headphones,
    /// The platform does not say (HDMI, USB DACs, line out…). Treated
    /// as speakers: a missing canceller sends everyone their own voice,
    /// an unneeded one costs one user a few ms.
    #[default]
    Unknown,
}

impl fmt::Display for OutputKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Speakers => "speakers",
            Self::Headphones => "headphones",
            Self::Unknown => "unknown output",
        })
    }
}

/// Should this session open its streams on the platform's voice-call
/// path? (`AEC-SPEC.md` §3.2.)
#[must_use]
pub fn wants_aec(mode: AecMode, output: OutputKind) -> bool {
    match mode {
        AecMode::Off => false,
        AecMode::Always => true,
        AecMode::Auto => !matches!(output, OutputKind::Headphones),
    }
}

/// The words [`AecReport::reason`] uses, so a UI can map them to its
/// own strings instead of showing these.
pub mod reasons {
    /// Not wanted: the setting is Off.
    pub const OFF: &str = "off";
    /// Not wanted: Auto, and the output is headphones.
    pub const HEADPHONES: &str = "headphones";
    /// On: the output is speakers.
    pub const SPEAKERS: &str = "speakers";
    /// On: the output did not say what it is.
    pub const UNKNOWN_OUTPUT: &str = "unknown output";
    /// On: the setting is Always (headphones or not).
    pub const ALWAYS: &str = "always";
    /// Wanted, but the device ships no echo canceller.
    pub const NO_CANCELLER: &str = "no canceller on this device";
    /// Wanted, but the voice-call stream would not open (or not as
    /// one); the log line before says why.
    pub const PATH_REFUSED: &str = "voice-call path refused";
    /// Wanted, but this platform has no voice-call path in baucord yet.
    pub const UNSUPPORTED: &str = "not on this platform yet";
}

/// Which canceller is running on the microphone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AecTier {
    /// The OS's own (Android's voice-communication chain, the Windows
    /// endpoint's AEC effect).
    Platform,
    /// None: not wanted, or wanted and not available.
    None,
}

/// What the engine got, refreshed on every stream (re)open
/// (`AEC-SPEC.md` §3.3). Read by the 10-second `audio stats:` line and
/// the settings page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AecReport {
    pub mode: AecMode,
    pub tier: AecTier,
    pub output: OutputKind,
    /// The render endpoint the canceller uses as its reference, when
    /// the platform lets us name it.
    pub reference: Option<String>,
    /// Why this tier, in a few log-friendly words.
    pub reason: &'static str,
}

impl AecReport {
    /// The report for a session that did not ask for the call path.
    #[must_use]
    pub fn not_wanted(mode: AecMode, output: OutputKind) -> Self {
        Self {
            mode,
            tier: AecTier::None,
            output,
            reference: None,
            reason: if mode == AecMode::Off {
                reasons::OFF
            } else {
                reasons::HEADPHONES
            },
        }
    }

    /// Short machine-ish form for logs: `platform (speakers)`,
    /// `none (headphones)`, `none (no canceller on this device)`.
    #[must_use]
    pub fn summary(&self) -> String {
        let tier = match self.tier {
            AecTier::Platform => "platform",
            AecTier::None => "none",
        };
        match &self.reference {
            Some(r) => format!("{tier} ({}) ref={r}", self.reason),
            None => format!("{tier} ({})", self.reason),
        }
    }
}

/// Android: classify the output devices `AudioManager.getDevices
/// (GET_DEVICES_OUTPUTS)` lists, by their `AudioDeviceInfo.TYPE_*`.
/// Android routes media to a personal-listening device whenever one is
/// connected, so any of those means headphones; an external device of
/// another kind means we can't tell; only built-ins means the
/// loudspeaker. Lives here, platform-free, so it is tested on the host.
#[must_use]
pub fn android_output_kind(device_types: &[i32]) -> OutputKind {
    // AudioDeviceInfo.TYPE_*: WIRED_HEADSET 3, WIRED_HEADPHONES 4,
    // BLUETOOTH_SCO 7, BLUETOOTH_A2DP 8, USB_HEADSET 22, HEARING_AID 23,
    // BLE_HEADSET 26.
    const PERSONAL: [i32; 7] = [3, 4, 7, 8, 22, 23, 26];
    // Built in, or not a sound outlet at all: BUILTIN_EARPIECE 1,
    // BUILTIN_SPEAKER 2, TELEPHONY 18, IP 20, BUS 21,
    // BUILTIN_SPEAKER_SAFE 24, REMOTE_SUBMIX 25, ECHO_REFERENCE 28.
    const INTERNAL: [i32; 8] = [1, 2, 18, 20, 21, 24, 25, 28];
    if device_types.iter().any(|t| PERSONAL.contains(t)) {
        OutputKind::Headphones
    } else if device_types.iter().any(|t| !INTERNAL.contains(t)) {
        // HDMI, line out, a USB DAC, a dock, a BLE speaker…
        OutputKind::Unknown
    } else {
        OutputKind::Speakers
    }
}

/// Windows: an endpoint's `PKEY_AudioEndpoint_FormFactor`
/// (`EndpointFormFactor`) as an [`OutputKind`] (AEC-SPEC.md §4.2).
#[must_use]
pub fn windows_output_kind(form_factor: u32) -> OutputKind {
    // Speakers 1; Headphones 3, Headset 5, Handset 6. Everything else —
    // remote network device, line level, S/PDIF, HDMI display, unknown —
    // says nothing about whether the microphone hears it.
    match form_factor {
        1 => OutputKind::Speakers,
        3 | 5 | 6 => OutputKind::Headphones,
        _ => OutputKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_follows_the_output_and_unknown_counts_as_speakers() {
        assert!(wants_aec(AecMode::Auto, OutputKind::Speakers));
        assert!(wants_aec(AecMode::Auto, OutputKind::Unknown));
        assert!(!wants_aec(AecMode::Auto, OutputKind::Headphones));
    }

    #[test]
    fn always_and_off_ignore_the_output() {
        for out in [
            OutputKind::Speakers,
            OutputKind::Headphones,
            OutputKind::Unknown,
        ] {
            assert!(wants_aec(AecMode::Always, out));
            assert!(!wants_aec(AecMode::Off, out));
        }
    }

    #[test]
    fn picker_index_round_trips() {
        for m in AecMode::ALL {
            assert_eq!(AecMode::from_index(m.index()), m);
        }
        assert_eq!(AecMode::from_index(99), AecMode::Auto);
    }

    #[test]
    fn android_outputs_classify() {
        // A phone with nothing plugged in: earpiece + speaker (+ the
        // telephony and echo-reference pseudo-devices some list).
        assert_eq!(android_output_kind(&[1, 2, 18, 28]), OutputKind::Speakers);
        assert_eq!(android_output_kind(&[2]), OutputKind::Speakers);
        // Wired headset, Bluetooth A2DP, a USB headset.
        assert_eq!(android_output_kind(&[1, 2, 3]), OutputKind::Headphones);
        assert_eq!(android_output_kind(&[1, 2, 8]), OutputKind::Headphones);
        assert_eq!(android_output_kind(&[2, 22]), OutputKind::Headphones);
        // HDMI or a plain USB device: can't tell.
        assert_eq!(android_output_kind(&[1, 2, 9]), OutputKind::Unknown);
        assert_eq!(android_output_kind(&[2, 11]), OutputKind::Unknown);
        // Nothing listed at all: the built-in path.
        assert_eq!(android_output_kind(&[]), OutputKind::Speakers);
    }

    #[test]
    fn windows_form_factors_classify() {
        assert_eq!(windows_output_kind(1), OutputKind::Speakers);
        assert_eq!(windows_output_kind(3), OutputKind::Headphones);
        assert_eq!(windows_output_kind(5), OutputKind::Headphones);
        assert_eq!(windows_output_kind(6), OutputKind::Headphones);
        // HDMI display (9), line level (2), unknown (10).
        for ff in [0, 2, 7, 8, 9, 10] {
            assert_eq!(windows_output_kind(ff), OutputKind::Unknown, "{ff}");
        }
    }

    #[test]
    fn report_summaries_read_as_the_spec_says() {
        let r = AecReport::not_wanted(AecMode::Auto, OutputKind::Headphones);
        assert_eq!(r.summary(), "none (headphones)");
        let r = AecReport::not_wanted(AecMode::Off, OutputKind::Speakers);
        assert_eq!(r.summary(), "none (off)");
        let r = AecReport {
            mode: AecMode::Auto,
            tier: AecTier::Platform,
            output: OutputKind::Speakers,
            reference: Some("Speakers (Realtek(R) Audio)".into()),
            reason: "speakers",
        };
        assert_eq!(
            r.summary(),
            "platform (speakers) ref=Speakers (Realtek(R) Audio)"
        );
    }
}
