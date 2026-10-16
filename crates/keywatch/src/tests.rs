//! The watch loop against a scripted keyboard: what it reports, and
//! how long it asks to sleep before it reads again.

use std::{cell::RefCell, collections::VecDeque};

use super::*;

const F7: u16 = 65;
const KEY_V: u16 = 47;
// Stand-ins for the modifier keys.
const CTRL: u16 = 1000;
const SHIFT: u16 = 1001;
const ALT: u16 = 1002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Mute,
    Talk,
}

/// What happens while the watcher sleeps once: the keys that are down
/// when it reads next, and what woke it.
struct Step {
    woke: Woke,
    down: Vec<u16>,
}

fn event(down: &[u16]) -> Step {
    Step {
        woke: Woke::Key,
        down: down.to_vec(),
    }
}

/// Time passed, or the keys changed with no event to say so.
fn quiet(down: &[u16]) -> Step {
    Step {
        woke: Woke::TimedOut,
        down: down.to_vec(),
    }
}

/// Something that is not a key of interest woke it (a message for a
/// window on Windows, a kick).
fn other(down: &[u16]) -> Step {
    Step {
        woke: Woke::Other,
        down: down.to_vec(),
    }
}

struct Scripted<'a> {
    events: bool,
    steps: VecDeque<Step>,
    down: Vec<u16>,
    stop: &'a AtomicBool,
    /// Every sleep the watcher asked for, in order.
    waits: Vec<Option<Duration>>,
}

impl Source for Scripted<'_> {
    fn refresh(&mut self) -> bool {
        true
    }

    fn key_down(&self, key: u16) -> bool {
        self.down.contains(&key)
    }

    fn modifier_down(&self, modifier: Modifier) -> bool {
        self.key_down(match modifier {
            Modifier::Ctrl => CTRL,
            Modifier::Shift => SHIFT,
            Modifier::Alt => ALT,
        })
    }

    fn has_events(&self) -> bool {
        self.events
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Woke {
        self.waits.push(timeout);
        let Some(step) = self.steps.pop_front() else {
            self.stop.store(true, Ordering::Release);
            return Woke::Other;
        };
        assert!(
            step.woke != Woke::TimedOut || timeout.is_some(),
            "a sleep with no timeout does not time out"
        );
        self.down = step.down;
        step.woke
    }
}

fn toggle(key: u16) -> Binding<Action> {
    Binding {
        action: Action::Mute,
        key,
        ctrl: false,
        shift: false,
        alt: false,
        hold: false,
    }
}

fn hold(key: u16) -> Binding<Action> {
    Binding {
        action: Action::Talk,
        hold: true,
        ..toggle(key)
    }
}

/// Run the loop over `steps`; what it reported and the sleeps it took.
fn run(
    events: bool,
    bindings: &[Binding<Action>],
    steps: Vec<Step>,
) -> (Vec<(Action, bool)>, Vec<Option<Duration>>) {
    run_after(&AtomicU8::new(0), events, bindings, steps)
}

/// [`run`], in a process whose events announced `announced` presses.
fn run_after(
    announced: &AtomicU8,
    events: bool,
    bindings: &[Binding<Action>],
    steps: Vec<Step>,
) -> (Vec<(Action, bool)>, Vec<Option<Duration>>) {
    let stop = AtomicBool::new(false);
    let mut source = Scripted {
        events,
        steps: steps.into(),
        down: Vec::new(),
        stop: &stop,
        waits: Vec::new(),
    };
    let reported = RefCell::new(Vec::new());
    watch(
        &mut source,
        bindings,
        &stop,
        announced,
        &|action, pressed| {
            reported.borrow_mut().push((action, pressed));
            true
        },
    );
    (reported.into_inner(), source.waits)
}

const TICK: Option<Duration> = Some(POLL);
const BEAT: Option<Duration> = Some(HEARTBEAT);

#[test]
fn edges_are_presses_for_toggles_and_both_for_holds() {
    let mut was = false;
    assert_eq!(edge(false, &mut was, false), None);
    assert_eq!(edge(false, &mut was, true), Some(true));
    assert_eq!(edge(false, &mut was, true), None);
    assert_eq!(edge(false, &mut was, false), None);
    assert!(!was, "the release is tracked even when not reported");

    let mut was = false;
    assert_eq!(edge(true, &mut was, true), Some(true));
    assert_eq!(edge(true, &mut was, true), None);
    assert_eq!(edge(true, &mut was, false), Some(false));
}

#[test]
fn without_events_the_keys_are_read_on_the_clock() {
    let (reported, waits) = run(
        false,
        &[toggle(F7), hold(KEY_V)],
        vec![
            quiet(&[F7]),
            quiet(&[F7]),
            quiet(&[]),
            quiet(&[KEY_V]),
            quiet(&[KEY_V]),
            quiet(&[]),
            quiet(&[]),
        ],
    );
    assert_eq!(
        reported,
        [
            (Action::Mute, true),
            (Action::Talk, true),
            (Action::Talk, false)
        ]
    );
    assert!(waits.iter().all(|w| *w == TICK), "{waits:?}");
}

#[test]
fn a_chord_needs_its_modifiers() {
    let chord = Binding {
        ctrl: true,
        shift: true,
        ..toggle(F7)
    };
    let (reported, _) = run(
        false,
        &[chord],
        vec![
            quiet(&[F7]),
            quiet(&[F7, CTRL]),
            quiet(&[F7, CTRL, ALT]),
            quiet(&[]),
            quiet(&[CTRL, SHIFT]),
            quiet(&[CTRL, SHIFT, F7]),
        ],
    );
    assert_eq!(reported, [(Action::Mute, true)]);
}

/// With events: a few reads at the start, then the heartbeat; a key
/// event brings a read and two more; a held binding is read on the
/// clock until it is let go.
#[test]
fn with_events_the_watcher_sleeps_until_a_key_and_reads_while_one_is_held() {
    let (reported, waits) = run(
        true,
        &[hold(KEY_V)],
        vec![
            // The first reads, an event's worth.
            quiet(&[]),
            quiet(&[]),
            quiet(&[]),
            // Asleep on the heartbeat; the key goes down.
            event(&[KEY_V]),
            // Held: read every tick, events or not.
            quiet(&[KEY_V]),
            quiet(&[KEY_V]),
            quiet(&[KEY_V]),
            quiet(&[KEY_V]),
            quiet(&[KEY_V]),
            // Let go; the two reads that follow an event.
            event(&[]),
            quiet(&[]),
            quiet(&[]),
            quiet(&[]),
        ],
    );
    assert_eq!(reported, [(Action::Talk, true), (Action::Talk, false)]);
    assert_eq!(
        waits,
        [
            TICK, TICK, TICK, // the start
            BEAT, // at rest
            TICK, TICK, TICK, TICK, TICK, TICK, // held
            TICK, TICK, TICK, // after the release's event
            BEAT, // at rest again (the script ends here)
        ]
    );
}

/// Three presses that events announced, and the heartbeat stops: the
/// watcher sleeps with no timeout at all.
#[test]
fn events_that_announce_their_presses_are_trusted_and_the_clock_stops() {
    let mut steps = vec![quiet(&[]), quiet(&[]), quiet(&[])];
    for _ in 0..PROVEN_PRESSES {
        // A tap: down, held for a tick, up, the reads after.
        steps.extend([
            event(&[F7]),
            quiet(&[F7]),
            event(&[]),
            quiet(&[]),
            quiet(&[]),
            quiet(&[]),
        ]);
    }
    // At rest, trusted: only an event ends this sleep.
    steps.extend([event(&[F7]), event(&[]), quiet(&[]), quiet(&[]), quiet(&[])]);
    let (reported, waits) = run(true, &[toggle(F7)], steps);
    assert_eq!(reported.len(), usize::from(PROVEN_PRESSES) + 1);
    assert!(reported.iter().all(|r| *r == (Action::Mute, true)));
    let at_rest: Vec<_> = waits.iter().filter(|w| **w != TICK).collect();
    assert_eq!(
        at_rest,
        [&BEAT, &BEAT, &BEAT, &None, &None],
        "a heartbeat before each of the first three presses, none after: {waits:?}"
    );
}

/// The safety net: a press the heartbeat finds, with no event before
/// it, means the events do not arrive. Reported (late), and from then
/// on the keys are read on the clock, as before 1.2.1.
#[test]
fn a_press_no_event_announced_sends_the_watcher_back_to_the_clock() {
    let (reported, waits) = run(
        true,
        &[toggle(F7), hold(KEY_V)],
        vec![
            quiet(&[]),
            quiet(&[]),
            quiet(&[]),
            // Asleep on the heartbeat; it ends with the key down.
            quiet(&[F7]),
            quiet(&[]),
            quiet(&[]),
            quiet(&[KEY_V]),
            quiet(&[]),
            quiet(&[]),
        ],
    );
    assert_eq!(
        reported,
        [
            (Action::Mute, true),
            (Action::Talk, true),
            (Action::Talk, false)
        ]
    );
    assert_eq!(&waits[..4], [TICK, TICK, TICK, BEAT]);
    assert!(waits[4..].iter().all(|w| *w == TICK), "{waits:?}");
}

/// A key that was down before the watcher began, or one found by the
/// reads while another binding is held, proves nothing either way.
#[test]
fn a_press_found_by_the_first_reads_or_during_a_hold_decides_nothing() {
    let mut steps = vec![
        // Down from the start: found by the first reads.
        quiet(&[KEY_V]),
        // Held; the toggle goes down with no event (found on a tick).
        quiet(&[KEY_V, F7]),
        quiet(&[KEY_V]),
        event(&[]),
        quiet(&[]),
        quiet(&[]),
        quiet(&[]),
    ];
    // Still on probation: the heartbeat, not the bare clock.
    steps.push(quiet(&[]));
    let (reported, waits) = run(true, &[toggle(F7), hold(KEY_V)], steps);
    assert_eq!(
        reported,
        [
            (Action::Talk, true),
            (Action::Mute, true),
            (Action::Talk, false)
        ]
    );
    assert!(
        matches!(waits.last(), Some(Some(beat)) if *beat >= HEARTBEAT),
        "{waits:?}"
    );
    assert_eq!(
        waits
            .iter()
            .filter(|w| w.is_some_and(|d| d >= HEARTBEAT))
            .count(),
        2
    );
}

/// At rest on probation, each quiet beat doubles the next, up to
/// [`HEARTBEAT_MAX`]; a key event starts it over.
#[test]
fn the_heartbeat_backs_off_while_nothing_happens() {
    let mut steps = vec![quiet(&[]), quiet(&[]), quiet(&[])];
    steps.extend((0..6).map(|_| quiet(&[])));
    steps.extend([event(&[F7]), event(&[]), quiet(&[]), quiet(&[]), quiet(&[])]);
    let (_, waits) = run(true, &[toggle(F7)], steps);
    let beats: Vec<_> = waits
        .iter()
        .filter_map(|w| w.filter(|d| *d != POLL))
        .map(|d| d.as_millis())
        .collect();
    assert_eq!(
        beats,
        [250, 500, 1000, 2000, 4000, 4000, 4000, 250],
        "{waits:?}"
    );
}

/// Woken by something that is not the time running out — Windows
/// wakes for any message — a press read then is no proof the events
/// failed: its own event may be on the way.
#[test]
fn a_press_read_after_some_other_wake_decides_nothing() {
    let (reported, waits) = run(
        true,
        &[toggle(F7)],
        vec![
            quiet(&[]),
            quiet(&[]),
            quiet(&[]),
            other(&[F7]),
            event(&[F7]),
            event(&[]),
            quiet(&[]),
            quiet(&[]),
            quiet(&[]),
        ],
    );
    assert_eq!(reported, [(Action::Mute, true)]);
    assert_eq!(waits.last(), Some(&BEAT), "still on probation: {waits:?}");
}

/// Proven once, proven for the process: the next watcher (a rebind, a
/// reconnect) sleeps on events alone from the start.
#[test]
fn a_later_watcher_trusts_events_already_proven() {
    let announced = AtomicU8::new(0);
    let mut steps = vec![quiet(&[]), quiet(&[]), quiet(&[])];
    for _ in 0..PROVEN_PRESSES {
        steps.extend([event(&[F7]), event(&[]), quiet(&[]), quiet(&[]), quiet(&[])]);
    }
    run_after(&announced, true, &[toggle(F7)], steps);
    assert_eq!(announced.load(Ordering::Relaxed), PROVEN_PRESSES);
    let (_, waits) = run_after(
        &announced,
        true,
        &[toggle(F7)],
        vec![quiet(&[]), quiet(&[]), quiet(&[])],
    );
    assert_eq!(waits, [TICK, TICK, TICK, None]);
}

#[test]
fn a_hold_still_held_when_the_watch_stops_is_released() {
    let (reported, _) = run(
        false,
        &[toggle(F7), hold(KEY_V)],
        vec![quiet(&[KEY_V, F7]), quiet(&[KEY_V, F7])],
    );
    assert_eq!(
        reported,
        [
            (Action::Mute, true),
            (Action::Talk, true),
            (Action::Talk, false)
        ]
    );
}

#[test]
fn the_watch_ends_when_nobody_listens() {
    let stop = AtomicBool::new(false);
    let mut source = Scripted {
        events: false,
        steps: vec![quiet(&[F7]), quiet(&[]), quiet(&[F7])].into(),
        down: Vec::new(),
        stop: &stop,
        waits: Vec::new(),
    };
    let calls = RefCell::new(0);
    watch(
        &mut source,
        &[toggle(F7)],
        &stop,
        &AtomicU8::new(0),
        &|_, _| {
            *calls.borrow_mut() += 1;
            false
        },
    );
    assert_eq!(*calls.borrow(), 1);
    assert_eq!(source.waits.len(), 1, "it stopped at the first refusal");
}

#[test]
fn only_the_bound_keys_and_their_modifiers_are_of_interest() {
    let plain = Interest::of(&[toggle(F7), hold(KEY_V)]);
    assert!(plain.wants(F7) && plain.wants(KEY_V));
    assert!(!plain.wants(30), "another key");
    assert!(!plain.wants_modifiers());

    let chord = Interest::of(&[Binding {
        alt: true,
        ..toggle(F7)
    }]);
    assert!(chord.wants(F7));
    assert!(chord.wants_modifiers());
}
