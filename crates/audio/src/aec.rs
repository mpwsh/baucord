//! Acoustic echo cancellation: the setting, the policy and the report
//! (`AEC-SPEC.md` §3, with the decisions in its §12).
//!
//! Platform first: the OS canceller already knows what the speakers
//! played and when, so the engine only asks for it and reports whether
//! it got it. Everything in this module is pure; the Windows backend
//! (`native::wasapi`) feeds it what it observed on the device.

use std::fmt;

/// The user's setting, `audio.echo_cancellation` in `settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum AecMode {
    /// Only when the output is not headphones.
    #[default]
    Auto,
    /// Whatever the output is.
    Always,
    /// Never: the streams open exactly as they did before 1.1.5.
    Off,
}

impl AecMode {
    /// The word `settings.json` and the logs use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Off => "off",
        }
    }
}

impl fmt::Display for AecMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the render endpoint says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Speakers,
    /// Headphones, a headset, or a handset: nothing the mic can hear.
    Headphones,
    /// Anything else (line out, HDMI, S/PDIF, a device that does not
    /// say). Treated as speakers.
    Unknown,
}

impl OutputKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Speakers => "speakers",
            Self::Headphones => "headphones",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for OutputKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether to ask the platform for echo cancellation.
///
/// `Unknown` counts as speakers: a missing canceller sends everyone
/// their own voice back, an unneeded one costs one user a few
/// milliseconds and possibly some ducking, and that user can pick Off.
///
/// ```
/// use audio::{wants_aec, AecMode, OutputKind};
/// assert!(wants_aec(AecMode::Auto, OutputKind::Unknown));
/// assert!(!wants_aec(AecMode::Auto, OutputKind::Headphones));
/// ```
#[must_use]
pub fn wants_aec(mode: AecMode, output: OutputKind) -> bool {
    match mode {
        AecMode::Off => false,
        AecMode::Always => true,
        AecMode::Auto => !matches!(output, OutputKind::Headphones),
    }
}

/// How much the backend could see of the platform canceller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecEvidence {
    /// Windows 11: the capture stream's effect list shows AEC, on.
    Listed,
    /// Windows 10: the stream is in the Communications category, which
    /// is what turns the OEM voice chain on, but there is no effect list
    /// to confirm AEC is part of it. Only a call on speakers tells.
    Unverified,
}

/// Which canceller runs on our capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecTier {
    /// The OS voice chain.
    Platform(AecEvidence),
    /// None at all.
    None,
}

/// Why the report reads the way it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecReason {
    // Why a canceller was asked for.
    Speakers,
    OutputUnknown,
    Always,
    // Why there is none.
    Off,
    Headphones,
    /// The effect list has no AEC for this microphone.
    NoEffect,
    /// AEC is listed but switched off.
    EffectOff,
    /// Windows would not put the stream in the Communications category.
    CategoryRefused,
    /// The native backend failed to open and cpal took over.
    CpalFallback,
    /// This platform has no backend that asks yet.
    NoPlatformCanceller,
    /// No capture stream is open.
    NoCapture,
}

impl AecReason {
    /// The words the `audio stats:` line and `/audio` print.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Speakers => "speakers",
            Self::OutputUnknown => "output unknown",
            Self::Always => "always",
            Self::Off => "off",
            Self::Headphones => "headphones",
            Self::NoEffect => "no effect on endpoint",
            Self::EffectOff => "effect switched off",
            Self::CategoryRefused => "category refused",
            Self::CpalFallback => "cpal fallback",
            Self::NoPlatformCanceller => "no platform canceller yet",
            Self::NoCapture => "no capture stream",
        }
    }
}

impl fmt::Display for AecReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the capture side got, refreshed on every open (spec D5): an
/// AEC that is not there is the next bug report, and this is what tells
/// the tiers apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AecReport {
    pub tier: AecTier,
    pub reason: AecReason,
    pub output: OutputKind,
    /// Name of the render endpoint the canceller should use as its
    /// reference, when known.
    pub reference: Option<String>,
    /// Windows accepted that endpoint as the reference explicitly
    /// (Windows 11 22H2 and later); otherwise the effect picks its own.
    pub reference_set: bool,
    /// "Default" meant the Default Communications Device for capture
    /// this session (spec D4).
    pub communications_defaults: bool,
}

impl AecReport {
    /// No canceller, for `reason`, with nothing else known.
    #[must_use]
    pub fn none(reason: AecReason) -> Self {
        Self {
            tier: AecTier::None,
            reason,
            output: OutputKind::Unknown,
            reference: None,
            reference_set: false,
            communications_defaults: false,
        }
    }

    /// The platform canceller was engaged (whether or not it could be
    /// confirmed).
    #[must_use]
    pub fn is_platform(&self) -> bool {
        matches!(self.tier, AecTier::Platform(_))
    }
}

impl Default for AecReport {
    fn default() -> Self {
        Self::none(AecReason::NoCapture)
    }
}

/// `platform(listed) ref=Speakers (Realtek(R) Audio)` / `none (headphones)`.
impl fmt::Display for AecReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let evidence = match self.tier {
            AecTier::None => return write!(f, "none ({})", self.reason),
            AecTier::Platform(AecEvidence::Listed) => "listed",
            AecTier::Platform(AecEvidence::Unverified) => "unverified",
        };
        let reference = self.reference.as_deref().unwrap_or("?");
        write!(f, "platform({evidence}) ref={reference}")
    }
}

/// The render endpoint the engine opened, as the capture side needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderEndpoint {
    /// The endpoint's `IMMDevice` id when the native WASAPI backend
    /// opened it — the only form Windows takes as an AEC reference.
    pub id: Option<String>,
    pub name: String,
    pub output: OutputKind,
    /// "Default" resolved to the Default Communications Device.
    pub communications_default: bool,
}

/// What the capture side is asked to do, decided from the setting and
/// the render endpoint that is already open (output opens first).
#[derive(Debug, Clone)]
pub(crate) struct AecPlan {
    pub(crate) mode: AecMode,
    pub(crate) output: OutputKind,
    /// Ask the platform: Communications category, and in AEC mode
    /// "Default" is the Default Communications Device.
    pub(crate) wanted: bool,
    pub(crate) render: Option<RenderEndpoint>,
}

impl AecPlan {
    /// No render endpoint (output failed, or not open) reads as
    /// `Unknown`, i.e. speakers.
    pub(crate) fn new(mode: AecMode, render: Option<RenderEndpoint>) -> Self {
        let output = render.as_ref().map_or(OutputKind::Unknown, |r| r.output);
        Self {
            mode,
            output,
            wanted: wants_aec(mode, output),
            render,
        }
    }

    /// Why the plan asks for a canceller, or why it does not.
    fn intent(&self) -> AecReason {
        match (self.mode, self.output) {
            (AecMode::Off, _) => AecReason::Off,
            (AecMode::Always, _) => AecReason::Always,
            (AecMode::Auto, OutputKind::Headphones) => AecReason::Headphones,
            (AecMode::Auto, OutputKind::Speakers) => AecReason::Speakers,
            (AecMode::Auto, OutputKind::Unknown) => AecReason::OutputUnknown,
        }
    }

    fn report(&self, tier: AecTier, reason: AecReason, reference_set: bool) -> AecReport {
        AecReport {
            tier,
            reason,
            output: self.output,
            reference: self.render.as_ref().map(|r| r.name.clone()),
            reference_set,
            communications_defaults: false,
        }
    }

    /// The report when nothing was asked for, if the plan asks nothing.
    pub(crate) fn declined(&self) -> Option<AecReport> {
        (!self.wanted).then(|| self.report(AecTier::None, self.intent(), false))
    }

    /// The report for a capture stream that could not ask (the cpal
    /// path, platforms without a backend yet).
    pub(crate) fn inactive(&self, reason: AecReason) -> AecReport {
        self.declined()
            .unwrap_or_else(|| self.report(AecTier::None, reason, false))
    }
}

/// What the Windows backend saw on the capture client after opening it.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectsSeen {
    /// No effects manager (Windows 10), or it failed: nothing to go on.
    Unavailable,
    /// The effect list has no AEC entry.
    NoAec,
    /// AEC is listed, switched off.
    AecOff,
    /// AEC is listed, on.
    AecOn,
}

#[cfg(any(windows, test))]
impl AecPlan {
    /// The tier, from what the backend observed after asking.
    pub(crate) fn classify(
        &self,
        category_set: bool,
        seen: EffectsSeen,
        reference_set: bool,
    ) -> AecReport {
        if let Some(report) = self.declined() {
            return report;
        }
        let (tier, reason) = if category_set {
            match seen {
                EffectsSeen::AecOn => (AecTier::Platform(AecEvidence::Listed), self.intent()),
                EffectsSeen::Unavailable => {
                    (AecTier::Platform(AecEvidence::Unverified), self.intent())
                }
                EffectsSeen::AecOff => (AecTier::None, AecReason::EffectOff),
                EffectsSeen::NoAec => (AecTier::None, AecReason::NoEffect),
            }
        } else {
            (AecTier::None, AecReason::CategoryRefused)
        };
        AecReport {
            communications_defaults: true,
            ..self.report(tier, reason, reference_set)
        }
    }
}

/// `EndpointFormFactor` values (`mmdeviceapi.h`; `windows` 0.62,
/// `Win32::Media::Audio`), as `PKEY_AudioEndpoint_FormFactor` stores
/// them (`VT_UI4`).
#[cfg(any(windows, test))]
mod form_factor {
    pub(super) const SPEAKERS: u32 = 1;
    pub(super) const HEADPHONES: u32 = 3;
    pub(super) const HEADSET: u32 = 5;
    pub(super) const HANDSET: u32 = 6;
}

/// Speakers are speakers; headphones, headsets and handsets are
/// headphones; line out, HDMI, S/PDIF and the rest are `Unknown`.
#[cfg(any(windows, test))]
pub(crate) fn output_kind_from_form_factor(value: u32) -> OutputKind {
    match value {
        form_factor::SPEAKERS => OutputKind::Speakers,
        form_factor::HEADPHONES | form_factor::HEADSET | form_factor::HANDSET => {
            OutputKind::Headphones
        }
        _ => OutputKind::Unknown,
    }
}

/// Whether a default-device change should reopen a render stream that
/// follows the default. Auto looks at both defaults (either can change
/// which one it lands on); Always lives on the communications default;
/// Off on the console default, as before. Platforms with one default
/// raise both flags.
pub(crate) fn render_follows_change(mode: AecMode, console: bool, communications: bool) -> bool {
    match mode {
        AecMode::Off => console,
        AecMode::Auto => console || communications,
        AecMode::Always => communications,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(output: OutputKind) -> RenderEndpoint {
        RenderEndpoint {
            id: Some("{0.0.0.00000000}.{speakers}".into()),
            name: "Speakers (Realtek(R) Audio)".into(),
            output,
            communications_default: true,
        }
    }

    fn plan_for(mode: AecMode, output: OutputKind) -> AecPlan {
        AecPlan::new(mode, Some(render(output)))
    }

    /// The common case: Auto, laptop speakers.
    fn on_speakers() -> AecPlan {
        plan_for(AecMode::Auto, OutputKind::Speakers)
    }

    mod wants_aec {
        use super::*;

        #[test]
        fn off_never_asks() {
            assert!(!wants_aec(AecMode::Off, OutputKind::Speakers));
        }

        #[test]
        fn always_asks_on_headphones() {
            assert!(wants_aec(AecMode::Always, OutputKind::Headphones));
        }

        #[test]
        fn auto_asks_on_speakers() {
            assert!(wants_aec(AecMode::Auto, OutputKind::Speakers));
        }

        #[test]
        fn auto_skips_headphones() {
            assert!(!wants_aec(AecMode::Auto, OutputKind::Headphones));
        }

        #[test]
        fn auto_treats_unknown_as_speakers() {
            assert!(wants_aec(AecMode::Auto, OutputKind::Unknown));
        }
    }

    mod plan_reports {
        use super::*;

        #[test]
        fn without_a_render_endpoint_reads_output_as_unknown() {
            assert_eq!(AecPlan::new(AecMode::Auto, None).output, OutputKind::Unknown);
        }

        #[test]
        fn without_a_render_endpoint_still_asks_in_auto() {
            assert!(AecPlan::new(AecMode::Auto, None).wanted);
        }

        #[test]
        fn inactive_on_headphones_says_headphones() {
            let plan = plan_for(AecMode::Auto, OutputKind::Headphones);
            assert_eq!(plan.inactive(AecReason::CpalFallback).reason, AecReason::Headphones);
        }

        #[test]
        fn inactive_on_speakers_keeps_the_given_reason() {
            let report = on_speakers().inactive(AecReason::CpalFallback);
            assert_eq!(report.reason, AecReason::CpalFallback);
        }

        #[test]
        fn inactive_carries_the_reference_name() {
            let report = on_speakers().inactive(AecReason::CpalFallback);
            assert_eq!(report.reference.as_deref(), Some("Speakers (Realtek(R) Audio)"));
        }
    }

    mod classify {
        use super::*;

        #[test]
        fn off_reports_off_whatever_was_seen() {
            let plan = plan_for(AecMode::Off, OutputKind::Speakers);
            let report = plan.classify(false, EffectsSeen::AecOn, false);
            assert_eq!((report.tier, report.reason), (AecTier::None, AecReason::Off));
        }

        #[test]
        fn listed_and_on_is_platform_listed() {
            let report = on_speakers().classify(true, EffectsSeen::AecOn, true);
            assert_eq!(report.tier, AecTier::Platform(AecEvidence::Listed));
        }

        #[test]
        fn no_effect_manager_is_platform_unverified() {
            let report = on_speakers().classify(true, EffectsSeen::Unavailable, false);
            assert_eq!(report.tier, AecTier::Platform(AecEvidence::Unverified));
        }

        #[test]
        fn listed_but_off_is_none() {
            let report = on_speakers().classify(true, EffectsSeen::AecOff, false);
            assert_eq!((report.tier, report.reason), (AecTier::None, AecReason::EffectOff));
        }

        #[test]
        fn missing_from_the_list_is_none() {
            let report = on_speakers().classify(true, EffectsSeen::NoAec, false);
            assert_eq!((report.tier, report.reason), (AecTier::None, AecReason::NoEffect));
        }

        #[test]
        fn a_refused_category_wins_over_the_list() {
            let report = on_speakers().classify(false, EffectsSeen::AecOn, false);
            assert_eq!(report.reason, AecReason::CategoryRefused);
        }

        #[test]
        fn platform_reason_says_why_it_was_asked() {
            let plan = AecPlan::new(AecMode::Auto, None);
            let report = plan.classify(true, EffectsSeen::AecOn, false);
            assert_eq!(report.reason, AecReason::OutputUnknown);
        }

        #[test]
        fn asking_marks_communications_defaults() {
            let report = on_speakers().classify(true, EffectsSeen::Unavailable, false);
            assert!(report.communications_defaults);
        }
    }

    mod report_display {
        use super::*;

        #[test]
        fn platform_prints_evidence_and_reference() {
            let report = on_speakers().classify(true, EffectsSeen::AecOn, true);
            assert_eq!(report.to_string(), "platform(listed) ref=Speakers (Realtek(R) Audio)");
        }

        #[test]
        fn none_prints_the_reason() {
            let report = AecReport::none(AecReason::Headphones);
            assert_eq!(report.to_string(), "none (headphones)");
        }
    }

    mod form_factor_mapping {
        use super::*;

        #[test]
        fn speakers_are_speakers() {
            assert_eq!(output_kind_from_form_factor(1), OutputKind::Speakers);
        }

        #[test]
        fn a_headset_is_headphones() {
            assert_eq!(output_kind_from_form_factor(5), OutputKind::Headphones);
        }

        #[test]
        fn hdmi_is_unknown() {
            // DigitalAudioDisplayDevice = 9
            assert_eq!(output_kind_from_form_factor(9), OutputKind::Unknown);
        }

        /// The constants are copied from the SDK; this keeps them honest
        /// on the one platform that has it.
        #[cfg(windows)]
        #[test]
        fn constants_match_the_windows_crate() {
            use windows::Win32::Media::Audio::{Handset, Headphones, Headset, Speakers};
            let sdk = [Speakers, Headphones, Headset, Handset]
                .map(|f| u32::try_from(f.0).expect("form factors are non-negative"));
            let ours = [
                form_factor::SPEAKERS,
                form_factor::HEADPHONES,
                form_factor::HEADSET,
                form_factor::HANDSET,
            ];
            assert_eq!(sdk, ours);
        }
    }

    mod default_changes {
        use super::*;

        #[test]
        fn off_ignores_the_communications_default() {
            assert!(!render_follows_change(AecMode::Off, false, true));
        }

        #[test]
        fn auto_follows_either_default() {
            assert!(render_follows_change(AecMode::Auto, false, true));
        }

        #[test]
        fn always_ignores_the_console_default() {
            assert!(!render_follows_change(AecMode::Always, true, false));
        }
    }
}
