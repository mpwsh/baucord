//! What the engine opens, closes and plans, and when — under each
//! platform's rules, on a machine that is not there (`fake`).
//!
//! Every test reads the machine's log: the plan as the platform was
//! told it, each stream opened (on which device, on the call path or
//! plain) and closed, in order. They were written against the engine
//! as it stood before its device handling was brought into one place
//! (review P2-55), to hold that work to what the six separate paths
//! did; they stay as the description of what it must keep doing.
//! A trace that changes is a behaviour that changed: on a desktop the
//! user hears a reopen as a gap, on a phone it moves the call's route.

use std::sync::atomic::Ordering;

use crate::{
    aec::{self, AecMode, OutputKind},
    fake::{Clock, Machine},
    route::{self, kinds, RouteDevice},
    StartOutcome, Traits, VoiceChat, VoiceChatConfig,
};

/// An engine on a machine, and a clock for its supervision.
struct Rig {
    // Dropped first: its streams close into the machine's log.
    vc: VoiceChat,
    clock: Clock,
    machine: Machine,
}

impl Rig {
    fn new(traits: Traits) -> Self {
        Self::on(Machine::new(traits))
    }

    fn on(machine: Machine) -> Self {
        Self {
            vc: VoiceChat::new(VoiceChatConfig::default()),
            clock: Clock::new(),
            machine,
        }
    }

    /// A desktop, started: both streams open.
    fn started(traits: Traits) -> Self {
        let mut rig = Self::new(traits);
        assert!(matches!(rig.vc.start(), StartOutcome::Started));
        rig.did();
        rig
    }

    /// A phone in a voice room, unmuted.
    fn in_a_room(machine: Machine) -> Self {
        let mut rig = Self::on(machine);
        rig.vc.set_presence(false, false);
        let _ = rig.vc.start();
        rig.vc.set_presence(true, true);
        rig.did();
        rig
    }

    /// What the engine did since the last look.
    fn did(&self) -> Vec<String> {
        self.machine.log()
    }

    /// One supervision tick, three seconds after the last: past the
    /// restart backoff and the device poll.
    fn tick(&mut self) -> bool {
        self.vc.service_at(self.clock.tick())
    }

    /// The open sides' devices, `(microphone, output)`.
    fn open(&self) -> (Option<&str>, Option<&str>) {
        (
            self.vc.input.as_ref().and(self.vc.opened_input.as_deref()),
            self.vc
                .output
                .as_ref()
                .and(self.vc.opened_output.as_deref()),
        )
    }

    fn report(&self) -> Option<String> {
        self.vc.aec_report().map(|r| r.summary())
    }

    fn fail_output(&self) {
        self.vc
            .stats
            .output_stream_failed
            .store(true, Ordering::Relaxed);
    }

    fn fail_input(&self) {
        self.vc
            .stats
            .input_stream_failed
            .store(true, Ordering::Relaxed);
    }
}

const NOTHING: [&str; 0] = [];

// ── The desktop without a voice-call path (Linux, macOS) ────────────────

#[test]
fn desktop_starts_playback_first_then_the_microphone() {
    let mut rig = Rig::new(Traits::DESKTOP);
    assert!(matches!(rig.vc.start(), StartOutcome::Started));
    assert_eq!(
        rig.did(),
        [
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
    assert_eq!(rig.open(), (Some("mic"), Some("speakers")));
    assert_eq!(
        rig.report().as_deref(),
        Some("none (not on this platform yet)")
    );
}

#[test]
fn desktop_keeps_its_streams_out_of_a_room_and_through_the_setting() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.vc.set_presence(false, true);
    rig.vc.set_presence(true, true);
    rig.vc.set_presence(true, false);
    rig.vc.set_echo_cancellation(AecMode::Always);
    rig.vc.set_echo_cancellation(AecMode::Off);
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
}

#[test]
fn desktop_without_a_microphone_plays_and_takes_it_when_it_comes() {
    let machine = Machine::new(Traits::DESKTOP);
    machine.set(|w| {
        w.inputs.clear();
        w.default_input = None;
    });
    let mut rig = Rig::on(machine);
    assert!(matches!(rig.vc.start(), StartOutcome::NoMicrophone(_)));
    assert_eq!(
        rig.did(),
        [
            "plan: plain streams",
            "open output speakers (plain)",
            "microphone refused: no default microphone",
        ]
    );
    // Asked again on every backoff, playback untouched.
    assert!(!rig.tick());
    assert_eq!(rig.did(), ["microphone refused: no default microphone"]);
    rig.machine.set(|w| {
        w.inputs.push("mic".into());
        w.default_input = Some("mic".into());
    });
    assert!(rig.tick());
    assert_eq!(rig.did(), ["open microphone mic (plain)"]);
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
}

#[test]
fn desktop_without_an_output_keeps_asking_for_one() {
    let machine = Machine::new(Traits::DESKTOP);
    machine.set(|w| {
        w.outputs.clear();
        w.default_output = None;
    });
    let mut rig = Rig::on(machine);
    assert!(matches!(rig.vc.start(), StartOutcome::NoPlayback(_)));
    assert_eq!(
        rig.did(),
        [
            "plan: plain streams",
            "output refused: no default output",
            "open microphone mic (plain)",
        ]
    );
    assert!(!rig.tick());
    assert_eq!(rig.did(), ["output refused: no default output"]);
    rig.machine.set(|w| {
        w.outputs.push("speakers".into());
        w.default_output = Some("speakers".into());
    });
    assert!(rig.tick());
    // The microphone that was fine is left alone.
    assert_eq!(rig.did(), ["open output speakers (plain)"]);
}

#[test]
fn desktop_reopens_only_the_side_whose_stream_failed() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.fail_output();
    assert!(rig.tick());
    assert_eq!(rig.did(), ["close output", "open output speakers (plain)"]);
    rig.fail_input();
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        ["close microphone", "open microphone mic (plain)"]
    );
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
}

#[test]
fn desktop_waits_out_the_backoff_between_restarts() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.fail_output();
    let now = rig.clock.tick();
    assert!(rig.vc.service_at(now));
    rig.did();
    // Failed again at once: not hammered.
    rig.fail_output();
    assert!(!rig.vc.service_at(now + std::time::Duration::from_secs(1)));
    assert_eq!(rig.did(), NOTHING);
    assert!(rig.vc.service_at(now + std::time::Duration::from_secs(2)));
    assert_eq!(rig.did(), ["close output", "open output speakers (plain)"]);
}

#[test]
fn desktop_follows_the_default_output_when_none_is_picked() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.machine.plug_output("headset", OutputKind::Headphones);
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING, "plugged in, not the default");
    rig.machine
        .set(|w| w.default_output = Some("headset".into()));
    assert!(rig.tick());
    assert_eq!(rig.did(), ["close output", "open output headset (plain)"]);
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
}

#[test]
fn desktop_follows_the_default_microphone_when_none_is_picked() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.machine.set(|w| {
        w.inputs.push("webcam".into());
        w.default_input = Some("webcam".into());
    });
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        ["close microphone", "open microphone webcam (plain)"]
    );
}

#[test]
fn desktop_leaves_a_picked_output_alone_when_the_default_moves() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.vc.set_output_device(Some("speakers")).expect("opens");
    assert_eq!(rig.did(), ["close output", "open output speakers (plain)"]);
    rig.machine.plug_output("headset", OutputKind::Headphones);
    rig.machine
        .set(|w| w.default_output = Some("headset".into()));
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
}

#[test]
fn desktop_waits_for_a_picked_output_that_was_unplugged() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.vc.set_output_device(Some("speakers")).expect("opens");
    rig.did();
    rig.machine.unplug_output("speakers");
    rig.fail_output();
    assert!(!rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close output",
            "output refused: output 'speakers' is not there"
        ]
    );
    assert_eq!(rig.open(), (Some("mic"), None));
    assert!(!rig.tick());
    assert_eq!(
        rig.did(),
        ["output refused: output 'speakers' is not there"]
    );
    rig.machine.plug_output("speakers", OutputKind::Speakers);
    assert!(rig.tick());
    assert_eq!(rig.did(), ["open output speakers (plain)"]);
}

#[test]
fn desktop_keeps_a_microphone_pick_that_is_not_there_and_takes_it_later() {
    let mut rig = Rig::started(Traits::DESKTOP);
    assert!(rig.vc.set_input_device(Some("usb mic")).is_err());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "microphone refused: microphone 'usb mic' is not there",
        ]
    );
    assert_eq!(rig.open(), (None, Some("speakers")));
    rig.machine.set(|w| w.inputs.push("usb mic".into()));
    assert!(rig.tick());
    assert_eq!(rig.did(), ["open microphone usb mic (plain)"]);
}

#[test]
fn a_new_output_pick_reopens_playback_alone_on_the_desktop() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.machine.plug_output("headset", OutputKind::Headphones);
    rig.vc.set_output_device(Some("headset")).expect("opens");
    assert_eq!(rig.did(), ["close output", "open output headset (plain)"]);
    // Asked for the same one again: reopened all the same.
    rig.vc.set_output_device(Some("headset")).expect("opens");
    assert_eq!(rig.did(), ["close output", "open output headset (plain)"]);
}

#[test]
fn desktop_with_both_streams_failed_reopens_the_microphone_then_playback() {
    let mut rig = Rig::started(Traits::DESKTOP);
    rig.fail_input();
    rig.fail_output();
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "open microphone mic (plain)",
            "close output",
            "open output speakers (plain)",
        ]
    );
}

#[test]
fn desktop_a_missing_playback_opens_before_a_failed_microphone_is_reopened() {
    let machine = Machine::new(Traits::DESKTOP);
    machine.set(|w| {
        w.outputs.clear();
        w.default_output = None;
    });
    let mut rig = Rig::on(machine);
    let _ = rig.vc.start();
    rig.did();
    rig.machine.set(|w| {
        w.outputs.push("speakers".into());
        w.default_output = Some("speakers".into());
    });
    rig.fail_input();
    assert!(rig.tick());
    // The room is silent until playback opens: that first.
    assert_eq!(
        rig.did(),
        [
            "open output speakers (plain)",
            "close microphone",
            "open microphone mic (plain)",
        ]
    );
}

#[test]
fn the_report_says_no_capture_once_a_picked_microphone_did_not_open() {
    let mut rig = Rig::started(Traits::WINDOWS);
    assert_eq!(
        rig.report().as_deref(),
        Some("platform (speakers) ref=speakers")
    );
    assert!(rig.vc.set_input_device(Some("usb mic")).is_err());
    // It kept the closed stream's words ("the canceller is running")
    // after a pick, though not after any other failed reopen: one of
    // the differences between the six places this used to live in.
    assert_eq!(rig.report(), None);
    rig.machine.set(|w| w.inputs.push("usb mic".into()));
    assert!(rig.tick());
    assert_eq!(
        rig.report().as_deref(),
        Some("platform (speakers) ref=speakers")
    );
}

#[test]
fn restarts_are_counted_one_a_side_and_one_for_a_new_plan() {
    let mut rig = Rig::started(Traits::WINDOWS);
    let restarts = |rig: &Rig| rig.vc.stats.stream_restarts.load(Ordering::Relaxed);
    assert_eq!(restarts(&rig), 0, "the first start is not a restart");
    rig.fail_output();
    rig.tick();
    assert_eq!(restarts(&rig), 1);
    rig.fail_input();
    rig.fail_output();
    rig.tick();
    assert_eq!(restarts(&rig), 3);
    rig.vc.set_echo_cancellation(AecMode::Off);
    assert_eq!(restarts(&rig), 4);
}

#[test]
fn closing_the_engine_closes_the_microphone_before_playback() {
    let rig = Rig::started(Traits::DESKTOP);
    let Rig { vc, machine, .. } = rig;
    drop(vc);
    assert_eq!(machine.log(), ["close microphone", "close output"]);
}

// ── Windows: the Communications category, a reference, two defaults ─────

#[test]
fn windows_on_speakers_takes_the_call_path_and_follows_the_communications_default() {
    let mut rig = Rig::new(Traits::WINDOWS);
    assert!(matches!(rig.vc.start(), StartOutcome::Started));
    assert_eq!(
        rig.did(),
        [
            "plan: call path",
            "open output speakers (call)",
            "open microphone mic (call), reference the communications default",
        ]
    );
    assert_eq!(
        rig.report().as_deref(),
        Some("platform (speakers) ref=speakers")
    );
}

#[test]
fn windows_on_headphones_stays_on_plain_streams() {
    let machine = Machine::new(Traits::WINDOWS);
    machine.set(|w| {
        w.kinds.insert("speakers".into(), OutputKind::Headphones);
    });
    let mut rig = Rig::on(machine);
    let _ = rig.vc.start();
    assert_eq!(
        rig.did(),
        [
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
    assert_eq!(rig.report().as_deref(), Some("none (headphones)"));
}

#[test]
fn windows_an_output_that_does_not_say_what_it_is_counts_as_speakers() {
    let machine = Machine::new(Traits::WINDOWS);
    machine.set(|w| {
        w.kinds.clear();
    });
    let mut rig = Rig::on(machine);
    let _ = rig.vc.start();
    assert_eq!(
        rig.report().as_deref(),
        Some("platform (unknown output) ref=speakers")
    );
}

#[test]
fn windows_picking_a_headset_leaves_the_call_path_on_both_sides() {
    let mut rig = Rig::started(Traits::WINDOWS);
    rig.machine.plug_output("headset", OutputKind::Headphones);
    rig.vc.set_output_device(Some("headset")).expect("opens");
    // Playback alone left capture with the canceller and the ducking
    // (review P1-18).
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output headset (plain)",
            "open microphone mic (plain)",
        ]
    );
    rig.vc.set_output_device(Some("speakers")).expect("opens");
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: call path",
            "open output speakers (call)",
            "open microphone mic (call), reference speakers",
        ]
    );
}

#[test]
fn windows_picking_other_speakers_moves_the_cancellers_reference() {
    let mut rig = Rig::started(Traits::WINDOWS);
    rig.machine.plug_output("monitor", OutputKind::Speakers);
    rig.vc.set_output_device(Some("monitor")).expect("opens");
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: call path",
            "open output monitor (call)",
            "open microphone mic (call), reference monitor",
        ]
    );
    assert_eq!(
        rig.report().as_deref(),
        Some("platform (speakers) ref=monitor")
    );
}

#[test]
fn windows_on_plain_streams_a_new_pick_of_the_same_kind_reopens_playback_alone() {
    let machine = Machine::new(Traits::WINDOWS);
    machine.set(|w| {
        w.kinds.insert("speakers".into(), OutputKind::Headphones);
    });
    machine.plug_output("headset", OutputKind::Headphones);
    let mut rig = Rig::on(machine);
    let _ = rig.vc.start();
    rig.did();
    rig.vc.set_output_device(Some("headset")).expect("opens");
    assert_eq!(rig.did(), ["close output", "open output headset (plain)"]);
}

#[test]
fn windows_the_setting_reopens_both_sides_only_when_the_plan_moves() {
    let mut rig = Rig::started(Traits::WINDOWS);
    rig.vc.set_echo_cancellation(AecMode::Always);
    assert_eq!(rig.did(), NOTHING, "speakers: Always is what Auto did");
    rig.vc.set_echo_cancellation(AecMode::Off);
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
    assert_eq!(rig.report().as_deref(), Some("none (off)"));
    rig.vc.set_echo_cancellation(AecMode::Auto);
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: call path",
            "open output speakers (call)",
            "open microphone mic (call), reference the communications default",
        ]
    );
}

#[test]
fn windows_off_means_the_console_default_and_the_others_the_communications_one() {
    let machine = Machine::new(Traits::WINDOWS);
    machine.plug_output("headset", OutputKind::Headphones);
    machine.set(|w| w.comms_output = Some("headset".into()));
    let mut rig = Rig::on(machine);
    let _ = rig.vc.start();
    // A headset set for calls beside speakers: Automatic reads and
    // opens the same device (review P1-18).
    assert_eq!(
        rig.did(),
        [
            "plan: plain streams",
            "open output headset (plain)",
            "open microphone mic (plain)",
        ]
    );
    rig.vc.set_echo_cancellation(AecMode::Off);
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
}

#[test]
fn windows_a_failed_output_on_the_same_device_reopens_playback_alone() {
    let mut rig = Rig::started(Traits::WINDOWS);
    rig.fail_output();
    assert!(rig.tick());
    assert_eq!(rig.did(), ["close output", "open output speakers (call)"]);
}

#[test]
fn windows_a_default_that_moved_to_a_headset_takes_both_sides_off_the_call_path() {
    let machine = Machine::new(Traits::WINDOWS);
    machine.set(|w| w.device_events = true);
    let mut rig = Rig::on(machine);
    let _ = rig.vc.start();
    rig.did();
    rig.machine.plug_output("headset", OutputKind::Headphones);
    rig.machine.set(|w| w.comms_output = Some("headset".into()));
    // Nothing is polled where Windows sends events...
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
    // ...and this is the event.
    rig.vc
        .stats
        .default_output_changed
        .store(true, Ordering::Relaxed);
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output headset (plain)",
            "open microphone mic (plain)",
        ]
    );
}

#[test]
fn windows_without_events_the_poll_finds_a_plan_gone_stale() {
    let mut rig = Rig::started(Traits::WINDOWS);
    // The same endpoint now says it is headphones (a jack that tells).
    rig.machine.set(|w| {
        w.kinds.insert("speakers".into(), OutputKind::Headphones);
    });
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
}

#[test]
fn windows_a_playback_that_comes_back_as_headphones_takes_both_sides_with_it() {
    let mut rig = Rig::started(Traits::WINDOWS);
    rig.vc.set_output_device(Some("speakers")).expect("opens");
    rig.did();
    rig.machine.unplug_output("speakers");
    rig.fail_output();
    assert!(!rig.tick());
    rig.did();
    // Back, and now it says it is a headset: the microphone is still
    // on the call path it was opened on.
    rig.machine.plug_output("speakers", OutputKind::Headphones);
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
}

#[test]
fn windows_with_both_devices_picked_the_setting_changes_only_the_report() {
    let mut rig = Rig::started(Traits::WINDOWS);
    rig.vc.set_output_device(Some("speakers")).expect("opens");
    rig.vc.set_input_device(Some("mic")).expect("opens");
    rig.machine.set(|w| {
        w.kinds.insert("speakers".into(), OutputKind::Headphones);
    });
    rig.tick();
    rig.did();
    assert_eq!(rig.report().as_deref(), Some("none (headphones)"));
    // Headphones under Automatic and Off are the same plain streams,
    // and nothing follows "Default": only the words change.
    rig.vc.set_echo_cancellation(AecMode::Off);
    assert_eq!(rig.did(), NOTHING);
    assert_eq!(rig.report().as_deref(), Some("none (off)"));
}

#[test]
fn windows_reports_a_microphone_without_a_canceller_and_ducking_it_cannot_refuse() {
    let machine = Machine::new(Traits::WINDOWS);
    machine.set(|w| {
        w.canceller = Err(aec::reasons::NO_CANCELLER);
        w.duck_opt_out = false;
    });
    let mut rig = Rig::on(machine);
    let _ = rig.vc.start();
    let report = rig.vc.aec_report().expect("a report");
    assert_eq!(report.summary(), "none (no canceller on this device)");
    assert!(
        report.lowers_others,
        "a call to Windows with or without a canceller"
    );
}

// ── Android: nothing open out of a room, routes, communication mode ─────

fn phone(headset: bool) -> Machine {
    let machine = Machine::new(Traits::ANDROID);
    machine.set(|w| {
        w.call_devices = Some(phone_routes(headset));
        w.output_types = vec![kinds::BUILTIN_EARPIECE, kinds::BUILTIN_SPEAKER];
        if headset {
            w.output_types.push(kinds::BLUETOOTH_SCO);
        }
    });
    machine
}

fn phone_routes(headset: bool) -> Vec<RouteDevice> {
    let mut routes = vec![
        RouteDevice {
            id: 1,
            kind: kinds::BUILTIN_EARPIECE,
            product: "Pixel".into(),
        },
        RouteDevice {
            id: 2,
            kind: kinds::BUILTIN_SPEAKER,
            product: "Pixel".into(),
        },
    ];
    if headset {
        routes.push(RouteDevice {
            id: 3,
            kind: kinds::BLUETOOTH_SCO,
            product: "Buds".into(),
        });
    }
    routes
}

#[test]
fn android_opens_nothing_in_the_lobby_and_asks_the_system_nothing() {
    let mut rig = Rig::on(phone(false));
    rig.vc.set_presence(false, false);
    assert!(matches!(rig.vc.start(), StartOutcome::Started));
    assert_eq!(rig.did(), ["plan: plain streams"]);
    assert_eq!(rig.open(), (None, None));
    let asked = rig.machine.surveys();
    for _ in 0..5 {
        assert!(!rig.tick());
    }
    assert_eq!(rig.did(), NOTHING);
    // Three device lists every 2 s, for hours of lobby (review P3-25).
    assert_eq!(rig.machine.surveys(), asked);
}

#[test]
fn android_joining_a_room_takes_the_call_path_on_the_loudspeaker() {
    let mut rig = Rig::on(phone(false));
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.did();
    rig.vc.set_presence(true, true);
    assert_eq!(
        rig.did(),
        [
            "plan: call path, communication mode on Speaker",
            "open output speakers (call)",
            "open microphone mic (call)",
        ]
    );
    assert_eq!(
        rig.report().as_deref(),
        Some("platform (speakers) ref=speakers")
    );
}

#[test]
fn android_muted_in_a_room_closes_the_microphone_alone() {
    let mut rig = Rig::in_a_room(phone(false));
    rig.vc.set_presence(true, false);
    assert_eq!(rig.did(), ["close microphone"]);
    assert_eq!(rig.report(), None);
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING, "closed on purpose: not retried");
    rig.vc.set_presence(true, true);
    assert_eq!(rig.did(), ["open microphone mic (call)"]);
}

#[test]
fn android_joining_muted_opens_playback_only() {
    let mut rig = Rig::on(phone(false));
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.did();
    rig.vc.set_presence(true, false);
    assert_eq!(
        rig.did(),
        [
            "plan: call path, communication mode on Speaker",
            "open output speakers (call)",
        ]
    );
}

#[test]
fn android_leaving_the_room_closes_everything_and_drops_communication_mode() {
    let mut rig = Rig::in_a_room(phone(false));
    rig.vc.set_presence(false, false);
    assert_eq!(
        rig.did(),
        ["close microphone", "close output", "plan: plain streams"]
    );
    assert_eq!(rig.open(), (None, None));
}

#[test]
fn android_a_headset_connecting_moves_both_sides_to_plain_streams() {
    let mut rig = Rig::in_a_room(phone(false));
    rig.machine.set(|w| {
        w.call_devices = Some(phone_routes(true));
        w.output_types.push(kinds::BLUETOOTH_SCO);
    });
    // Nothing fails when Android reroutes: only the poll can tell.
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
    assert_eq!(rig.report().as_deref(), Some("none (headphones)"));
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
}

#[test]
fn android_picking_a_route_replans_and_a_repeated_pick_does_nothing() {
    let mut rig = Rig::in_a_room(phone(true));
    assert_eq!(rig.report().as_deref(), Some("none (headphones)"));
    rig.vc
        .set_output_device(Some(route::SPEAKER))
        .expect("kept");
    // The loudspeaker over a connected headset is reachable only in
    // communication mode.
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: call path, communication mode on Speaker",
            "open output speakers (call)",
            "open microphone mic (call)",
        ]
    );
    // The UI's device poll repeats the current choice.
    rig.vc
        .set_output_device(Some(route::SPEAKER))
        .expect("kept");
    assert_eq!(rig.did(), NOTHING);
}

#[test]
fn android_a_picked_route_holds_communication_mode_with_cancellation_off() {
    let mut rig = Rig::in_a_room(phone(true));
    rig.vc
        .set_output_device(Some(route::SPEAKER))
        .expect("kept");
    rig.did();
    rig.vc.set_echo_cancellation(AecMode::Off);
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams, communication mode on Speaker",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
}

#[test]
fn android_a_route_picked_in_the_lobby_is_applied_at_the_join() {
    let mut rig = Rig::on(phone(true));
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.did();
    rig.vc
        .set_output_device(Some(route::EARPIECE))
        .expect("kept");
    rig.vc.set_input_device(None).expect("kept");
    assert_eq!(rig.did(), NOTHING);
    rig.vc.set_presence(true, true);
    assert_eq!(
        rig.did(),
        [
            "plan: call path, communication mode on Phone earpiece",
            "open output speakers (call)",
            "open microphone mic (call)",
        ]
    );
}

#[test]
fn android_a_microphone_not_granted_yet_is_asked_for_while_it_is_wanted() {
    let machine = phone(false);
    machine.set(|w| {
        w.inputs.clear();
        w.default_input = None;
    });
    let mut rig = Rig::on(machine);
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.vc.set_presence(true, true);
    assert_eq!(
        rig.did(),
        [
            "plan: plain streams",
            "plan: call path, communication mode on Speaker",
            "open output speakers (call)",
            "microphone refused: no default microphone",
        ]
    );
    assert!(!rig.tick());
    assert_eq!(rig.did(), ["microphone refused: no default microphone"]);
    // Muted meanwhile: it should stay closed, and nothing asks.
    rig.vc.set_presence(true, false);
    assert!(!rig.tick());
    assert_eq!(rig.did(), NOTHING);
    // The permission came, and the user unmutes.
    rig.machine.set(|w| {
        w.inputs.push("mic".into());
        w.default_input = Some("mic".into());
    });
    rig.vc.set_presence(true, true);
    assert_eq!(rig.did(), ["open microphone mic (call)"]);
}

#[test]
fn android_a_failed_playback_in_a_room_is_reopened_alone() {
    let mut rig = Rig::in_a_room(phone(false));
    rig.fail_output();
    assert!(rig.tick());
    assert_eq!(rig.did(), ["close output", "open output speakers (call)"]);
}

#[test]
fn android_the_setting_in_the_lobby_waits_for_the_join() {
    let mut rig = Rig::on(phone(false));
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.did();
    let asked = rig.machine.surveys();
    rig.vc.set_echo_cancellation(AecMode::Off);
    assert_eq!(rig.did(), NOTHING);
    assert_eq!(rig.machine.surveys(), asked, "nothing open: nothing asked");
    rig.vc.set_presence(true, true);
    assert_eq!(
        rig.did(),
        [
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
}

// ── iOS: one unit for both sides on the call path, an output always ─────

#[test]
fn ios_in_the_lobby_plays_through_a_plain_output_and_records_nothing() {
    let mut rig = Rig::new(Traits::IOS);
    rig.vc.set_presence(false, false);
    assert!(matches!(rig.vc.start(), StartOutcome::Started));
    // An app with no audio running is suspended in the background.
    assert_eq!(
        rig.did(),
        ["plan: plain streams", "open output speakers (plain)"]
    );
    assert_eq!(rig.open(), (None, Some("speakers")));
}

#[test]
fn ios_joining_a_room_moves_both_sides_onto_the_voice_unit() {
    let mut rig = Rig::new(Traits::IOS);
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.did();
    rig.vc.set_presence(true, true);
    assert_eq!(
        rig.did(),
        [
            "close output",
            "plan: call path, unit opened live",
            "open output speakers (call)",
            "open microphone mic (call)",
            "unit microphone live",
        ]
    );
}

#[test]
fn ios_joining_muted_opens_the_unit_muted_with_both_sides_on_it() {
    let mut rig = Rig::new(Traits::IOS);
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.did();
    rig.vc.set_presence(true, false);
    assert_eq!(
        rig.did(),
        [
            "close output",
            "plan: call path, unit opened muted",
            "open output speakers (call)",
            "open microphone mic (call)",
            "unit microphone muted",
        ]
    );
}

#[test]
fn ios_mutes_on_the_unit_without_closing_anything() {
    let mut rig = Rig::in_a_room(Machine::new(Traits::IOS));
    rig.vc.set_presence(true, false);
    assert_eq!(rig.did(), ["unit microphone muted"]);
    assert_eq!(rig.open(), (Some("mic"), Some("speakers")));
    rig.vc.set_presence(true, true);
    assert_eq!(rig.did(), ["unit microphone live"]);
}

#[test]
fn ios_an_interruption_reopens_both_sides_of_the_unit_together() {
    let mut rig = Rig::in_a_room(Machine::new(Traits::IOS));
    rig.fail_output();
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: call path, unit opened live",
            "open output speakers (call)",
            "open microphone mic (call)",
            "unit microphone live",
        ]
    );
}

#[test]
fn ios_headphones_take_both_sides_off_the_unit_and_mute_then_closes_the_microphone() {
    let mut rig = Rig::in_a_room(Machine::new(Traits::IOS));
    rig.machine.set(|w| w.route_kind = OutputKind::Headphones);
    assert!(rig.tick());
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
    // No unit to mute on: closed like Android's.
    rig.vc.set_presence(true, false);
    assert_eq!(rig.did(), ["close microphone"]);
    rig.vc.set_presence(true, true);
    assert_eq!(rig.did(), ["open microphone mic (plain)"]);
}

#[test]
fn ios_leaving_the_room_goes_back_to_the_plain_output() {
    let mut rig = Rig::in_a_room(Machine::new(Traits::IOS));
    rig.vc.set_presence(false, false);
    assert_eq!(
        rig.did(),
        [
            "close microphone",
            "close output",
            "plan: plain streams",
            "open output speakers (plain)",
        ]
    );
}

#[test]
fn ios_a_unit_that_will_not_open_leaves_both_sides_on_plain_streams() {
    let machine = Machine::new(Traits::IOS);
    machine.set(|w| w.unit_refuses = true);
    let mut rig = Rig::on(machine);
    rig.vc.set_presence(false, false);
    let _ = rig.vc.start();
    rig.did();
    rig.vc.set_presence(true, true);
    assert_eq!(
        rig.did(),
        [
            "close output",
            "plan: call path refused, plain streams",
            "open output speakers (plain)",
            "open microphone mic (plain)",
        ]
    );
    assert_eq!(
        rig.report().as_deref(),
        Some("none (voice-call path refused)")
    );
}
