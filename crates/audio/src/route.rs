//! Where call audio plays on a phone (ANDROID-SPEC.md D31) — the
//! platform-free half, tested on the host. The Java half is
//! `native::android` (`AudioManager.getAvailableCommunicationDevices`
//! and `setCommunicationDevice`).
//!
//! On Android the voice-call path (`aec`) puts the phone in
//! communication mode, and in that mode the OS routes *everything* —
//! the call and any media, a watched stream's sound included — to the
//! communication device, which is the earpiece unless the app picks
//! another. So the app picks: the loudspeaker by default, a headset
//! whenever one is connected, or whatever the user chose in Settings →
//! Audio → Output, which on a phone lists these routes instead of
//! sound cards.

use crate::aec::OutputKind;

/// The `AudioDeviceInfo.TYPE_*` values routing deals in.
pub mod kinds {
    pub const BUILTIN_EARPIECE: i32 = 1;
    pub const BUILTIN_SPEAKER: i32 = 2;
    pub const WIRED_HEADSET: i32 = 3;
    pub const WIRED_HEADPHONES: i32 = 4;
    pub const BLUETOOTH_SCO: i32 = 7;
    pub const BLUETOOTH_A2DP: i32 = 8;
    pub const USB_DEVICE: i32 = 11;
    pub const USB_HEADSET: i32 = 22;
    pub const HEARING_AID: i32 = 23;
    pub const BLE_HEADSET: i32 = 26;
    pub const BLE_SPEAKER: i32 = 27;
}

/// The picker's name for the loudspeaker (the frontend translates it).
pub const SPEAKER: &str = "Speaker";
/// The picker's name for the earpiece (the frontend translates it).
pub const EARPIECE: &str = "Phone earpiece";
/// The picker's name for a headset on a cable (the frontend translates
/// it; a cable reports the phone's own model as its product).
pub const WIRED: &str = "Wired headset";

/// One place call audio can go, as the platform lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteDevice {
    /// `AudioDeviceInfo.getId()`, for the log.
    pub id: i32,
    /// `AudioDeviceInfo.getType()`.
    pub kind: i32,
    /// `AudioDeviceInfo.getProductName()` — the phone's model for
    /// built-in devices, the headset's name for Bluetooth.
    pub product: String,
}

/// External devices, in the order the automatic choice prefers them:
/// a cable beats a radio, a headset with a microphone beats one without.
const PERSONAL: [i32; 9] = [
    kinds::WIRED_HEADSET,
    kinds::USB_HEADSET,
    kinds::WIRED_HEADPHONES,
    kinds::BLE_HEADSET,
    kinds::BLUETOOTH_SCO,
    kinds::HEARING_AID,
    kinds::BLUETOOTH_A2DP,
    kinds::USB_DEVICE,
    kinds::BLE_SPEAKER,
];

/// The name the picker shows and the setting stores.
#[must_use]
pub fn label(device: &RouteDevice) -> String {
    let named = |generic: &str| {
        let product = device.product.trim();
        if product.is_empty() {
            generic.to_string()
        } else {
            product.to_string()
        }
    };
    match device.kind {
        kinds::BUILTIN_SPEAKER => SPEAKER.to_string(),
        kinds::BUILTIN_EARPIECE => EARPIECE.to_string(),
        // A cable reports the phone's own model as its product.
        kinds::WIRED_HEADSET | kinds::WIRED_HEADPHONES => WIRED.to_string(),
        kinds::USB_HEADSET | kinds::USB_DEVICE => named("USB headset"),
        kinds::BLUETOOTH_SCO | kinds::BLUETOOTH_A2DP | kinds::BLE_HEADSET | kinds::BLE_SPEAKER => {
            named("Bluetooth headset")
        }
        kinds::HEARING_AID => named("Hearing aid"),
        _ => named("Audio device"),
    }
}

/// The device the automatic choice takes: a connected headset, else
/// the loudspeaker, else whatever is first.
#[must_use]
fn auto_choice(devices: &[RouteDevice]) -> Option<usize> {
    PERSONAL
        .iter()
        .find_map(|kind| devices.iter().position(|d| d.kind == *kind))
        .or_else(|| {
            devices
                .iter()
                .position(|d| d.kind == kinds::BUILTIN_SPEAKER)
        })
        .or(if devices.is_empty() { None } else { Some(0) })
}

/// The device a saved selection means: the one with that label, or the
/// automatic choice when there is no selection or it is not connected.
#[must_use]
pub fn choose(selection: Option<&str>, devices: &[RouteDevice]) -> Option<usize> {
    selection
        .and_then(|name| devices.iter().position(|d| label(d) == name))
        .or_else(|| auto_choice(devices))
}

/// What a route means for echo cancellation (`aec::wants_aec`): the
/// loudspeaker and the earpiece are heard by the microphone (and the
/// earpiece is reachable only on the call path anyway); a headset is
/// not.
#[must_use]
pub fn output_kind(kind: i32) -> OutputKind {
    match kind {
        kinds::BUILTIN_SPEAKER | kinds::BUILTIN_EARPIECE => OutputKind::Speakers,
        kinds::WIRED_HEADSET
        | kinds::WIRED_HEADPHONES
        | kinds::USB_HEADSET
        | kinds::BLE_HEADSET
        | kinds::BLUETOOTH_SCO
        | kinds::BLUETOOTH_A2DP
        | kinds::HEARING_AID => OutputKind::Headphones,
        // A Bluetooth speaker, a USB DAC, HDMI: the room may hear it.
        _ => OutputKind::Unknown,
    }
}

/// The selection's device when it is connected (`None`: Automatic,
/// or the chosen device is gone and Automatic stands in).
#[must_use]
pub fn explicit(selection: Option<&str>, devices: &[RouteDevice]) -> Option<usize> {
    selection.and_then(|name| devices.iter().position(|d| label(d) == name))
}

/// What the call's output is, for echo cancellation and for whether
/// communication mode is needed at all: the picked device's kind, or
/// Automatic's — except that Automatic leaves media-only Bluetooth
/// alone. Headphones that only speak A2DP never appear among the
/// communication devices, so Automatic would pick the loudspeaker and
/// pull the call (and the stream's sound) off them; with A2DP among the
/// outputs, Automatic is headphones (plain streams, the system routes
/// media to them). `output_types` is `AudioManager.getDevices(OUTPUTS)`.
#[must_use]
pub fn effective_output_kind(
    explicit: Option<usize>,
    devices: &[RouteDevice],
    output_types: &[i32],
) -> OutputKind {
    if let Some(i) = explicit {
        return output_kind(devices[i].kind);
    }
    let auto = auto_choice(devices);
    let built_in = auto.is_none_or(|i| {
        matches!(
            devices[i].kind,
            kinds::BUILTIN_SPEAKER | kinds::BUILTIN_EARPIECE
        )
    });
    if built_in && output_types.contains(&kinds::BLUETOOTH_A2DP) {
        return OutputKind::Headphones;
    }
    match auto {
        Some(i) => output_kind(devices[i].kind),
        None => crate::aec::android_output_kind(output_types),
    }
}

/// The output picker's rows: one per label, the automatic choice
/// marked as the default.
#[must_use]
pub fn picker(devices: &[RouteDevice]) -> Vec<(String, bool)> {
    let auto = auto_choice(devices);
    let mut rows: Vec<(String, bool)> = Vec::new();
    for (i, device) in devices.iter().enumerate() {
        let name = label(device);
        let is_auto = auto == Some(i);
        if let Some(row) = rows.iter_mut().find(|(n, _)| *n == name) {
            // A headset listed twice (Bluetooth SCO and LE): one row.
            row.1 |= is_auto;
        } else {
            rows.push((name, is_auto));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: i32, kind: i32, product: &str) -> RouteDevice {
        RouteDevice {
            id,
            kind,
            product: product.to_string(),
        }
    }

    fn phone() -> Vec<RouteDevice> {
        vec![
            dev(1, kinds::BUILTIN_EARPIECE, "Pixel 8"),
            dev(2, kinds::BUILTIN_SPEAKER, "Pixel 8"),
        ]
    }

    #[test]
    fn a_bare_phone_plays_on_the_loudspeaker() {
        let devices = phone();
        assert_eq!(choose(None, &devices), Some(1));
        assert_eq!(output_kind(devices[1].kind), OutputKind::Speakers);
    }

    #[test]
    fn a_connected_headset_wins_the_automatic_choice() {
        let mut devices = phone();
        devices.push(dev(9, kinds::BLE_HEADSET, "Pixel Buds Pro"));
        assert_eq!(choose(None, &devices), Some(2));
        assert_eq!(output_kind(kinds::BLE_HEADSET), OutputKind::Headphones);
        // A cable beats a radio.
        devices.push(dev(10, kinds::WIRED_HEADSET, "Pixel 8"));
        assert_eq!(choose(None, &devices), Some(3));
    }

    #[test]
    fn a_selection_is_honoured_while_connected() {
        let mut devices = phone();
        devices.push(dev(9, kinds::BLUETOOTH_SCO, "Pixel Buds Pro"));
        assert_eq!(choose(Some(EARPIECE), &devices), Some(0));
        assert_eq!(choose(Some(SPEAKER), &devices), Some(1));
        assert_eq!(choose(Some("Pixel Buds Pro"), &devices), Some(2));
        // Gone: back to the automatic choice.
        assert_eq!(choose(Some("Sony WH-1000XM5"), &phone()), Some(1));
    }

    #[test]
    fn labels_name_built_ins_by_role_and_headsets_by_product() {
        assert_eq!(label(&dev(1, kinds::BUILTIN_SPEAKER, "Pixel 8")), SPEAKER);
        assert_eq!(label(&dev(2, kinds::BUILTIN_EARPIECE, "Pixel 8")), EARPIECE);
        assert_eq!(
            label(&dev(3, kinds::WIRED_HEADSET, "Pixel 8")),
            "Wired headset"
        );
        assert_eq!(label(&dev(4, kinds::BLE_HEADSET, "Buds")), "Buds");
        assert_eq!(
            label(&dev(5, kinds::BLUETOOTH_SCO, " ")),
            "Bluetooth headset"
        );
    }

    #[test]
    fn the_picker_lists_each_route_once_and_marks_the_automatic_one() {
        let mut devices = phone();
        devices.push(dev(9, kinds::BLUETOOTH_SCO, "Buds"));
        devices.push(dev(11, kinds::BLE_HEADSET, "Buds"));
        let rows = picker(&devices);
        assert_eq!(
            rows,
            vec![
                (EARPIECE.to_string(), false),
                (SPEAKER.to_string(), false),
                ("Buds".to_string(), true),
            ]
        );
        assert_eq!(picker(&[]), Vec::<(String, bool)>::new());
        assert_eq!(choose(None, &[]), None);
    }

    #[test]
    fn automatic_leaves_media_only_bluetooth_alone() {
        let devices = phone();
        // Pixel Buds in A2DP only: not a communication device.
        let outputs = [1, 2, kinds::BLUETOOTH_A2DP];
        assert_eq!(
            effective_output_kind(None, &devices, &outputs),
            OutputKind::Headphones
        );
        // Picking the speaker over them is honoured.
        let pick = explicit(Some(SPEAKER), &devices);
        assert_eq!(
            effective_output_kind(pick, &devices, &outputs),
            OutputKind::Speakers
        );
        // Without them: the speaker.
        assert_eq!(
            effective_output_kind(None, &devices, &[1, 2]),
            OutputKind::Speakers
        );
        // A gone pick is Automatic.
        assert_eq!(explicit(Some("Buds"), &devices), None);
    }

    #[test]
    fn unknown_kinds_are_unknown_outputs() {
        assert_eq!(output_kind(9), OutputKind::Unknown); // HDMI
        assert_eq!(output_kind(kinds::BLE_SPEAKER), OutputKind::Unknown);
        assert_eq!(output_kind(kinds::USB_HEADSET), OutputKind::Headphones);
    }
}
