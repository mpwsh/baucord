//! A stand-in machine for the engine's own tests: its devices, what
//! the platform says about them, and a log of everything the engine
//! did to it.
//!
//! `VoiceChat` decides which streams are open, on which device and on
//! which path, from six entry points and under four platforms' rules.
//! None of that could be run before: a test machine has no sound card,
//! and three of the platforms are not the one the tests run on. With a
//! [`Machine`] installed on the test's thread, the engine takes its
//! [`Traits`] from it, asks it instead of the platform, and "opens"
//! [`Side`]s that hold no stream — while every decision above that line
//! is the code that ships (review P2-55, PRODUCTION-PLAN TST-2).
//!
//! Nothing here is compiled outside `cfg(test)`.

use std::{
    cell::RefCell,
    collections::HashMap,
    error::Error,
    fmt::Write as _,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use crate::{
    aec::{AecMode, OutputKind},
    route::{self, RouteDevice},
    stats::AudioStats,
    Survey, Traits,
};

thread_local! {
    static WORLD: RefCell<Option<World>> = const { RefCell::new(None) };
}

/// The platforms, by their rules.
impl Traits {
    /// Linux and macOS today: no voice-call path, defaults followed.
    pub(crate) const DESKTOP: Self = Self {
        voice_path: false,
        releases_mic: false,
        shared_unit: false,
        routes: false,
        reference: false,
        follows_default: true,
    };
    pub(crate) const WINDOWS: Self = Self {
        voice_path: true,
        reference: true,
        ..Self::DESKTOP
    };
    pub(crate) const ANDROID: Self = Self {
        voice_path: true,
        releases_mic: true,
        shared_unit: false,
        routes: true,
        reference: false,
        follows_default: false,
    };
    pub(crate) const IOS: Self = Self {
        voice_path: true,
        releases_mic: true,
        shared_unit: true,
        routes: false,
        reference: false,
        follows_default: false,
    };
}

/// The machine's state. Tests change it through [`Machine::set`].
pub(crate) struct World {
    pub traits: Traits,
    /// Microphones and outputs that are there, by name.
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// The OS defaults (the console ones on Windows).
    pub default_input: Option<String>,
    pub default_output: Option<String>,
    /// Windows: the Default Communications Devices.
    pub comms_input: Option<String>,
    pub comms_output: Option<String>,
    /// What each output says it is; one not listed says nothing.
    pub kinds: HashMap<String, OutputKind>,
    /// iOS: the session's route.
    pub route_kind: OutputKind,
    /// Android: the devices a call can use, and every output's type.
    pub call_devices: Option<Vec<RouteDevice>>,
    pub output_types: Vec<i32>,
    /// iOS: the voice-processing unit will not open.
    pub unit_refuses: bool,
    /// Windows: default-device notifications arrive (no polling).
    pub device_events: bool,
    /// What a capture on the call path finds for a canceller.
    pub canceller: Result<(), &'static str>,
    /// Windows: the playback stream may ask not to lower other apps.
    pub duck_opt_out: bool,
    /// How often the engine asked where the sound goes.
    surveys: usize,
    log: Vec<String>,
}

/// Installs a [`World`] on this thread for as long as it lives.
pub(crate) struct Machine;

impl Machine {
    /// A machine with one microphone and one output, both the default.
    pub(crate) fn new(traits: Traits) -> Self {
        let world = World {
            traits,
            inputs: vec!["mic".into()],
            outputs: vec!["speakers".into()],
            default_input: Some("mic".into()),
            default_output: Some("speakers".into()),
            comms_input: None,
            comms_output: None,
            kinds: HashMap::from([("speakers".to_string(), OutputKind::Speakers)]),
            route_kind: OutputKind::Speakers,
            call_devices: None,
            output_types: Vec::new(),
            unit_refuses: false,
            device_events: false,
            canceller: Ok(()),
            duck_opt_out: true,
            surveys: 0,
            log: Vec::new(),
        };
        WORLD.with_borrow_mut(|slot| *slot = Some(world));
        Self
    }

    /// Change the machine.
    #[allow(clippy::unused_self)]
    pub(crate) fn set(&self, change: impl FnOnce(&mut World)) {
        WORLD.with_borrow_mut(|slot| change(slot.as_mut().expect("a machine")));
    }

    /// An output appears (not as anybody's default).
    pub(crate) fn plug_output(&self, name: &str, kind: OutputKind) {
        self.set(|w| {
            w.outputs.push(name.to_string());
            w.kinds.insert(name.to_string(), kind);
        });
    }

    /// An output goes away; a default that was it is left with none.
    pub(crate) fn unplug_output(&self, name: &str) {
        self.set(|w| {
            w.outputs.retain(|o| o != name);
            for slot in [&mut w.default_output, &mut w.comms_output] {
                if slot.as_deref() == Some(name) {
                    *slot = None;
                }
            }
        });
    }

    /// How many times the engine has asked the platform about the
    /// output (each is device lists on Android, the route on iOS).
    #[allow(clippy::unused_self)]
    pub(crate) fn surveys(&self) -> usize {
        WORLD.with_borrow(|slot| slot.as_ref().expect("a machine").surveys)
    }

    /// Everything the engine did since the last call, in order.
    #[allow(clippy::unused_self)]
    pub(crate) fn log(&self) -> Vec<String> {
        WORLD.with_borrow_mut(|slot| std::mem::take(&mut slot.as_mut().expect("a machine").log))
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        WORLD.with_borrow_mut(|slot| *slot = None);
    }
}

/// A stream that is not one: how it was opened, and a line in the log
/// when it closes.
pub(crate) struct Side {
    what: &'static str,
    pub name: Option<String>,
    pub verdict: Option<Result<(), &'static str>>,
    pub reference_named: bool,
    pub lowers_others: bool,
}

impl Drop for Side {
    fn drop(&mut self) {
        note(format!("close {}", self.what));
    }
}

fn with<R>(read: impl FnOnce(&mut World) -> R) -> Option<R> {
    WORLD.with_borrow_mut(|slot| slot.as_mut().map(read))
}

/// A machine is installed on this thread.
pub(crate) fn active() -> bool {
    WORLD.with_borrow(Option::is_some)
}

pub(crate) fn traits() -> Option<Traits> {
    with(|w| w.traits)
}

/// A line in the machine's log (nothing without a machine).
pub(crate) fn note(line: String) {
    with(|w| w.log.push(line));
}

/// `VoiceChat::survey`'s answer here.
pub(crate) fn survey(selection: Option<&str>, mode: AecMode) -> Option<Survey> {
    with(|w| {
        w.surveys += 1;
        if w.traits.routes {
            // Android's own arithmetic (`android_output_kind`), on this
            // machine's lists.
            let output = match w.call_devices.as_deref() {
                Some(devices) => route::effective_output_kind(
                    route::explicit(selection, devices),
                    devices,
                    &w.output_types,
                ),
                None => crate::aec::android_output_kind(&w.output_types),
            };
            return Survey {
                output,
                devices: w.call_devices.clone(),
            };
        }
        let output = if w.traits.shared_unit {
            w.route_kind
        } else {
            // "Default" on Windows: the communications device unless
            // echo cancellation is Off.
            let comms = w.traits.reference && mode != AecMode::Off;
            let name = selection.map(String::from).or_else(|| {
                if comms {
                    w.comms_output.clone().or_else(|| w.default_output.clone())
                } else {
                    w.default_output.clone()
                }
            });
            name.and_then(|n| w.kinds.get(&n).copied())
                .unwrap_or(OutputKind::Unknown)
        };
        Survey {
            output,
            devices: None,
        }
    })
}

/// `VoiceChat::engage_call_path` here: the plan as the platform is
/// told it, and whether the call path can be had.
pub(crate) fn engage(
    wanted: bool,
    comm: bool,
    route: Option<&str>,
    unit_muted: bool,
) -> Option<bool> {
    with(|w| {
        let granted = !(w.traits.shared_unit && w.unit_refuses);
        let mut line = format!(
            "plan: {}",
            if wanted && granted {
                "call path"
            } else if wanted {
                "call path refused, plain streams"
            } else {
                "plain streams"
            }
        );
        if comm {
            let _ = write!(
                line,
                ", communication mode on {}",
                route.unwrap_or("the system's route")
            );
        }
        if w.traits.shared_unit && wanted && granted {
            line.push_str(if unit_muted {
                ", unit opened muted"
            } else {
                ", unit opened live"
            });
        }
        w.log.push(line);
        granted
    })
}

fn resolve(
    what: &str,
    selection: Option<&str>,
    present: &[String],
    default: Option<&String>,
) -> Result<String, Box<dyn Error>> {
    match selection {
        Some(name) if present.iter().any(|p| p == name) => Ok(name.to_string()),
        Some(name) => Err(format!("{what} '{name}' is not there").into()),
        None => default
            .filter(|d| present.contains(d))
            .cloned()
            .ok_or_else(|| format!("no default {what}").into()),
    }
}

/// `VoiceChat::open_playback` here. A stream that starts clears its
/// "failed" flag, as the real ones do in `start()`; where the device
/// is looked up before that (`cpal`, everywhere but Windows) a missing
/// device fails earlier and leaves the flag as it was.
pub(crate) fn open_playback(
    selection: Option<&str>,
    voice: bool,
    comms: bool,
    stats: &AudioStats,
) -> Option<Result<Side, Box<dyn Error>>> {
    with(|w| {
        let default = if comms {
            w.comms_output.as_ref().or(w.default_output.as_ref())
        } else {
            w.default_output.as_ref()
        };
        let opened = resolve("output", selection, &w.outputs, default);
        if opened.is_ok() || w.traits.reference {
            stats.output_stream_failed.store(false, Ordering::Relaxed);
        }
        match opened {
            Ok(name) => {
                w.log.push(format!(
                    "open output {name} ({})",
                    if voice { "call" } else { "plain" }
                ));
                Ok(Side {
                    what: "output",
                    name: Some(name),
                    verdict: None,
                    reference_named: false,
                    lowers_others: w.traits.reference && voice && !w.duck_opt_out,
                })
            }
            Err(e) => {
                w.log.push(format!("output refused: {e}"));
                Err(e)
            }
        }
    })
}

/// `VoiceChat::open_capture` here. `reference` is the output the
/// engine names for the canceller where the platform takes one.
pub(crate) fn open_capture(
    selection: Option<&str>,
    voice: bool,
    comms: bool,
    reference: Option<&str>,
    stats: &AudioStats,
) -> Option<Result<Side, Box<dyn Error>>> {
    with(|w| {
        let default = if comms {
            w.comms_input.as_ref().or(w.default_input.as_ref())
        } else {
            w.default_input.as_ref()
        };
        let opened = resolve("microphone", selection, &w.inputs, default);
        if opened.is_ok() || w.traits.reference {
            stats.input_stream_failed.store(false, Ordering::Relaxed);
        }
        match opened {
            Ok(name) => {
                let mut line = format!(
                    "open microphone {name} ({})",
                    if voice { "call" } else { "plain" }
                );
                if voice && w.traits.reference {
                    let _ = write!(
                        line,
                        ", reference {}",
                        reference.unwrap_or("the communications default")
                    );
                }
                w.log.push(line);
                Ok(Side {
                    what: "microphone",
                    name: Some(name),
                    verdict: voice.then_some(w.canceller),
                    reference_named: !w.traits.reference || voice,
                    lowers_others: false,
                })
            }
            Err(e) => {
                w.log.push(format!("microphone refused: {e}"));
                Err(e)
            }
        }
    })
}

pub(crate) fn has_device_events() -> Option<bool> {
    with(|w| w.device_events)
}

// Outer: is there a machine; inner: does it have a default.
#[allow(clippy::option_option)]
pub(crate) fn default_input_name() -> Option<Option<String>> {
    with(|w| w.default_input.clone())
}

#[allow(clippy::option_option)]
pub(crate) fn default_output_name() -> Option<Option<String>> {
    with(|w| w.default_output.clone())
}

/// A clock for `service_at`: each step is past the restart backoff and
/// the device poll.
pub(crate) struct Clock(Instant);

impl Clock {
    pub(crate) fn new() -> Self {
        Self(Instant::now())
    }

    /// The next supervision tick, three seconds on.
    pub(crate) fn tick(&mut self) -> Instant {
        self.0 += Duration::from_secs(3);
        self.0
    }
}
