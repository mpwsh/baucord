//! Audio-device hot-reload diffing.
//!
//! The 2.5 s device poll's decision logic, extracted pure from the FLTK
//! `app.rs::handle_devices_changed`: given the previous and current
//! device snapshots plus the user's saved selections, produce the chat
//! notices to show and the rebuild decisions to apply. The shell owns
//! the side effects (voice-thread messages, WASAPI loopback restart,
//! settings writes, localized strings).

/// A notice for chat, pre-localization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceNotice {
    MicConnected(String),
    MicDisconnected(String),
    SpeakerConnected(String),
    SpeakerDisconnected(String),
    /// The user's *selected* input device disappeared.
    InputLost,
    /// Following the system default and it changed (`.0` = new default,
    /// `None` if the OS won't say).
    InputDefaultChanged(Option<String>),
    OutputLost,
    OutputDefaultChanged(Option<String>),
}

/// What the shell must do after a diff.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeviceDiff {
    pub notices: Vec<DeviceNotice>,
    /// The output voice plays on moved: the default changed with nothing
    /// picked, or the picked output came or went. A running share's
    /// sound capture is restarted on it. The engine follows the default
    /// by itself, and the shell stands the default in for a picked
    /// device while it is unplugged (review 2026-10, P2-9): this asked
    /// it to clear the pick and reopen, a second time.
    pub output_moved: bool,
    /// Nothing changed at all (early-out for the caller).
    pub unchanged: bool,
}

/// Extract the name of whichever device is marked as the OS default.
#[must_use]
pub(crate) fn default_device(devices: &[(String, bool)]) -> Option<String> {
    devices
        .iter()
        .find(|(_, is_default)| *is_default)
        .map(|(name, _)| name.clone())
}

/// Diff two device snapshots.
///
/// `selected_input` / `selected_output` are the user's saved device
/// names (`None` = follow the system default), exactly as stored in
/// `Settings.audio.{input,output}.device`.
#[must_use]
pub fn diff(
    old_input: &[(String, bool)],
    new_input: &[(String, bool)],
    old_output: &[(String, bool)],
    new_output: &[(String, bool)],
    selected_input: Option<&str>,
    selected_output: Option<&str>,
) -> DeviceDiff {
    use std::collections::HashSet;

    let old_in: HashSet<&str> = old_input.iter().map(|(n, _)| n.as_str()).collect();
    let new_in: HashSet<&str> = new_input.iter().map(|(n, _)| n.as_str()).collect();
    let old_out: HashSet<&str> = old_output.iter().map(|(n, _)| n.as_str()).collect();
    let new_out: HashSet<&str> = new_output.iter().map(|(n, _)| n.as_str()).collect();

    let old_in_default = default_device(old_input);
    let new_in_default = default_device(new_input);
    let old_out_default = default_device(old_output);
    let new_out_default = default_device(new_output);

    if old_in == new_in
        && old_out == new_out
        && old_in_default == new_in_default
        && old_out_default == new_out_default
    {
        return DeviceDiff {
            unchanged: true,
            ..DeviceDiff::default()
        };
    }

    let mut notices = Vec::new();
    for name in new_in.difference(&old_in) {
        notices.push(DeviceNotice::MicConnected((*name).into()));
    }
    for name in old_in.difference(&new_in) {
        notices.push(DeviceNotice::MicDisconnected((*name).into()));
    }
    for name in new_out.difference(&old_out) {
        notices.push(DeviceNotice::SpeakerConnected((*name).into()));
    }
    for name in old_out.difference(&new_out) {
        notices.push(DeviceNotice::SpeakerDisconnected((*name).into()));
    }

    // Said when it happens, once: a pick that went away (not at every
    // change while it stays away), a default that moved.
    match selected_input {
        Some(sel) if old_in.contains(sel) && !new_in.contains(sel) => {
            notices.push(DeviceNotice::InputLost);
        }
        None if old_in_default != new_in_default => {
            notices.push(DeviceNotice::InputDefaultChanged(new_in_default));
        }
        _ => {}
    }
    let output_moved = match selected_output {
        Some(sel) => old_out.contains(sel) != new_out.contains(sel),
        None => old_out_default != new_out_default,
    };
    match selected_output {
        Some(sel) if old_out.contains(sel) && !new_out.contains(sel) => {
            notices.push(DeviceNotice::OutputLost);
        }
        None if output_moved => {
            notices.push(DeviceNotice::OutputDefaultChanged(new_out_default));
        }
        _ => {}
    }

    DeviceDiff {
        notices,
        output_moved,
        unchanged: false,
    }
}

/// Names from `saved` selections that are missing from `present` — the
/// connect-view "saved device isn't connected" warning.
#[must_use]
pub fn missing_saved<'a>(
    saved_input: Option<&'a str>,
    saved_output: Option<&'a str>,
    input_devices: &[(String, bool)],
    output_devices: &[(String, bool)],
) -> (Option<&'a str>, Option<&'a str>) {
    let missing_in = saved_input.filter(|name| !input_devices.iter().any(|(n, _)| n == name));
    let missing_out = saved_output.filter(|name| !output_devices.iter().any(|(n, _)| n == name));
    (missing_in, missing_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(name, is_default)` — the shape `audio::VoiceChat::list_*_devices` returns.
    fn devs(names: &[(&str, bool)]) -> Vec<(String, bool)> {
        names.iter().map(|(n, d)| ((*n).into(), *d)).collect()
    }

    #[test]
    fn identical_snapshots_report_unchanged() {
        let a = devs(&[("Mic", true)]);
        let b = devs(&[("Spk", true)]);
        assert!(diff(&a, &a, &b, &b, None, None).unchanged);
    }

    #[test]
    fn unplugging_selected_input_notes_the_loss_once() {
        let old = devs(&[("USB Mic", false), ("Built-in", true)]);
        let new = devs(&[("Built-in", true)]);
        let out = devs(&[("Spk", true)]);
        let d = diff(&old, &new, &out, &out, Some("USB Mic"), None);
        assert!(d.notices.contains(&DeviceNotice::InputLost));
        assert!(d
            .notices
            .contains(&DeviceNotice::MicDisconnected("USB Mic".into())));
        assert!(!d.output_moved);
        // Another device comes while it is away: no second "lost".
        let more = devs(&[("Built-in", true), ("Webcam", false)]);
        let d = diff(&new, &more, &out, &out, Some("USB Mic"), None);
        assert!(!d.notices.contains(&DeviceNotice::InputLost));
    }

    #[test]
    fn a_default_change_is_said_when_following_the_default() {
        let inp = devs(&[("Mic", true)]);
        let old_out = devs(&[("A", true), ("B", false)]);
        let new_out = devs(&[("A", false), ("B", true)]);
        let d = diff(&inp, &inp, &old_out, &new_out, None, None);
        assert!(d.output_moved);
        assert!(d
            .notices
            .contains(&DeviceNotice::OutputDefaultChanged(Some("B".into()))));
        // A picked output that stays is not moved by the default.
        let d = diff(&inp, &inp, &old_out, &new_out, None, Some("A"));
        assert!(!d.output_moved);
        assert!(d.notices.is_empty());
    }

    #[test]
    fn missing_saved_reports_only_absent_devices() {
        let inp = devs(&[("Mic", true)]);
        let out = devs(&[("Spk", true)]);
        let (mi, mo) = missing_saved(Some("Blue Yeti"), Some("Spk"), &inp, &out);
        assert_eq!(mi, Some("Blue Yeti"));
        assert_eq!(mo, None);
    }
}
