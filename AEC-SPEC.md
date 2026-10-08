# baucord — Acoustic echo cancellation (AEC) technical spec

Status: draft 1, 2026-09-15; implemented 2026-10-06 (§12) — Windows
(Communications category), Android (voice-call streams with
`AcousticEchoCanceler`, E9) and iOS (the VPIO unit, E10) as of 1.1.7.
macOS, Linux and the stream's own audio are open (PRODUCTION-PLAN.md
PRD-5, MOB-12). Start from `START-HERE.md`. Decisions in §1 are
taken; §12 collects the ones taken after this draft, the way
`PERSISTENCE-SPEC.md` §12 does.

## 0. Purpose and scope

People on laptops using the built-in speakers and microphone send
everyone else their own voices back. baucord has no echo cancellation
today — not in software, and not from the OS, because of one omission
(§2). This spec adds AEC with a **platform-first** design: every OS
ships a tuned canceller that already has the hard part solved — the
reference signal, i.e. knowing exactly what the speakers played and
when — so the app asks the platform first and only carries its own
canceller where the platform has none.

Windows first (§4), because it is where the users are and where the
change is smallest. The contract (§3) is what macOS, Linux and Android
implement later (§9); Android's `VoiceCommunication` path in
`ANDROID-SPEC.md` §6 is this contract's third implementation.

Out of scope: noise suppression and AGC beyond what the platform chain
does (RNNoise stays as it is, §3.4), a software canceller in this
release (§8 says how that decision gets made), any change to the
encoder ladder.

## 1. Decisions

| # | decision | why |
|---|---|---|
| D1 | Platform AEC first; software AEC only as a measured fallback | the platform has the reference signal and the tuning; we have neither |
| D2 | One setting, `audio.echo_cancellation`: **Auto** (default) / Always / Off | Auto = only when the output is speakers; nobody on headphones sees any change |
| D3 | In AEC mode the streams are opened in Windows' **Communications** category; otherwise the category stays as today (`Other`) | that category is what turns the OS voice chain on — and what triggers Windows' "reduce other sounds" ducking, which headphone users must not get |
| D4 | In AEC mode "Default" resolves to the **Default Communications Device**; otherwise the console default, as today | it is the device Windows users assign to calls; Discord and Teams follow it |
| D5 | The active tier is *verified*, not assumed, and reported in `/audio`, the log and telemetry | an AEC that isn't there is the next bug report; the report is what tells tier 2 apart from "works" |
| D6 | RNNoise stays on by default; the setting is unchanged | the OEM chain's own NS is device-dependent; removing ours on faith would regress quiet-room users |
| D7 | The fallback (§8) is chosen after the tier reports from the users' machines, not now | two candidates with different costs; the probe data decides |

## 2. What happens today

`audio/src/native/wasapi.rs` opens capture and render with
`initialize_client` and never sets a stream category, so the client is
`AudioCategory_Other`. Windows applies its voice-processing chain —
the OEM's APOs (Realtek, Intel, Synaptics, Cirrus ship AEC/NS/AGC on
nearly every laptop) and, on Windows 11, Microsoft's own where the
driver opted in — **only to streams in the `Communications` category**.
So no AEC ever runs on our microphone, whatever the laptop has. Echo
follows: the far end's voice leaves the speakers, enters the mic, is
encoded and sent back to them 100–200 ms later.

Headset users never hear this, which is why it has been tolerable.

## 3. The contract, in the `audio` crate

### 3.1 Setting

`AudioSettings.echo_cancellation: AecMode` with
`enum AecMode { Auto, Always, Off }`, `#[serde(default)]` → `Auto`,
serialised as the lowercase word, in `settings.json` under `audio`.
Settings → Audio gets a picker beside the device pickers; changing it
reopens both streams through the existing device-change path.

### 3.2 Policy (pure, unit-tested)

```rust
pub enum OutputKind { Speakers, Headphones, Unknown }   // from the render endpoint

pub fn wants_aec(mode: AecMode, output: OutputKind) -> bool {
    match mode {
        AecMode::Off    => false,
        AecMode::Always => true,
        AecMode::Auto   => !matches!(output, OutputKind::Headphones),
    }
}
```

`Unknown` counts as speakers: a device that does not say what it is
gets AEC, because the failure mode of a missing canceller (echo for
everyone) is worse than the failure mode of an unneeded one (a few ms
of latency and possible ducking for one user, who can pick Off).

### 3.3 What the backend reports

`AudioStats` gains:

```rust
pub struct AecReport {
    pub tier: AecTier,               // Platform { vendor }, Software(kind) [later], None
    pub reference: Option<String>,   // the render endpoint used as reference, if known
    pub output: OutputKind,
    pub reason: &'static str,        // why this tier: "headphones", "off", "no effect on endpoint", …
}
pub enum AecVendor { Inbox, Oem, Unknown }
```

Filled when the streams open, cleared when they close, refreshed on
every reopen. `voice.stats_summary()` appends
`aec: platform(oem) ref=Speakers (Realtek(R) Audio)` /
`aec: none (headphones)` / `aec: none (no effect on endpoint)` to the
10-second `audio stats:` line, and the settings page shows the same
words under the picker so the user can see what they got.

### 3.4 The processing chain

Unchanged: platform AEC (when active) → our capture ring → RNNoise →
VAD → Opus. The platform chain may add its own NS and AGC ahead of
RNNoise; that is accepted for this release (D6) and revisited only
with a recording that shows it hurting.

## 4. Windows

All against `wasapi` 0.24, which the backend already uses and which
wraps every call below. Verified in its source, not from memory.

### 4.1 Open sequence (capture)

```
1. device      = enumerator.get_default_device_for_role(Capture, role)     // role: §4.3
                 or the named device, as today
2. client      = device.get_iaudioclient()
3. if wants_aec:
     client.set_properties(AudioClientProperties::new()
                           .set_category(AudioCategory_Communications))     // BEFORE initialize
4. client.initialize_client(...)                                            // existing, with the
                                                                            // autoconvert fallback
5. if wants_aec:
     match client.get_aec_control() {                                       // Windows 11 22H2+
       Ok(ctl)  => ctl.set_echo_cancellation_render_endpoint(&render_id),  // our output device
       Err(E_NOINTERFACE) => {}                                             // older Windows, or the
                                                                            // effect picks its own
       Err(e)   => log, continue
     }
     tier = match client.get_audio_effects_manager()
                        .and_then(|m| m.get_audio_effects()) {
       Ok(Some(effects)) if effects has ACOUSTIC_ECHO_CANCELLATION enabled
                          => Platform { vendor: Oem | Inbox (§4.4) }
       Ok(_)              => None ("no effect on endpoint")
       Err(_)             => Unknown-vendor Platform if the category was set
                             (Windows 10: no effects manager, effects may still run)
     }
6. proceed as today (capture client, event handle, worker thread)
```

Step 3 is the whole fix on most laptops. Step 5 is what makes D5 true.

Render opens the same way with the same category decision, so that
Windows sees one communications session (ducking and communications
routing behave coherently) — verify on the first laptop that the OEM
render chain does not change the sound audibly when the category
flips; if it does, render stays `Other` and §12 records it.

### 4.2 Output kind

`OutputKind` comes from the render endpoint's
`PKEY_AudioEndpoint_FormFactor` (`Speakers`, `Headphones`, `Headset`,
`LineLevel`, `SPDIF`, `HDMI`, `UnknownFormFactor`…):
Headphones/Headset → `Headphones`; Speakers → `Speakers`; everything
else → `Unknown` (→ speakers, §3.2). The `wasapi` crate reads
properties but keeps the accessor private, so the backend adds a
20-line helper using the `windows` crate directly
(`IMMDeviceEnumerator::GetDevice(id)` → `OpenPropertyStore` →
`GetValue`), pinned to the **same `windows` version `wasapi` 0.24
depends on (0.62)** so there is one windows tree, the rule `video`
already follows.

### 4.3 Default devices

`wants_aec` → `Role::Communications`; otherwise `Role::Console` as
today. The Sound panel shows these as two separate "Default" entries;
the settings page's device list keeps one "Default" row and the
tooltip says which one applies.

### 4.4 Vendor

`AUDIO_EFFECT` carries the effect's CLSID. Microsoft's inbox AEC has
a known CLSID; anything else is `Oem`. If the effects manager is
absent (Windows 10) the vendor is `Unknown`. This is reporting only;
nothing branches on it.

### 4.5 Device and setting changes

The existing default-device and device-loss paths reopen the streams;
the open sequence re-evaluates `wants_aec` from the *new* render
endpoint every time, so plugging in headphones drops out of AEC mode
and unplugging them re-enters it, with the stats line saying so. When
only the render device changes, the capture client's reference
endpoint is updated with `set_echo_cancellation_render_endpoint`
without reopening capture (the call is valid any time after
`Initialize`); if that fails, reopen.

### 4.6 Ducking

A Communications session makes Windows apply the user's "When Windows
detects communications activity" setting (default: reduce other sounds
by 80 %). Auto confines this to speaker users. The setting's help
string says it plainly, in all three catalogs: "On speakers, Windows
may lower other apps while you talk (Sound settings →
Communications)." Since E11 each voice stream also asks Windows not to
duck others (`IAudioClientDuckingControl`, Windows 11); the stream's
log line says whether it was granted. Users still ducked: Sound
settings → Communications → "Do nothing" (`control mmsys.cpl,,3`).

### 4.7 Latency

The platform chain adds processing delay on capture (typically
10–20 ms). The jitter buffer and the ladder do not care; the
mute/unmute and PTT paths do not either. Noted so nobody chases it as
a regression: the `audio stats:` line already prints capture latency.

## 5. Settings UI and strings

Settings → Audio: **Echo cancellation** picker (Auto / Always / Off)
under the device pickers, with the one-line status from §3.3 beneath
it (`aec-status-*` strings). Settings → General: **Log level** picker
(error / warn / info / debug / trace), writing `logging.level` and
applied live (`log::set_max_level` plus the file logger's filter);
`RUST_LOG` still wins at startup when set. Every new string in the
three Fluent catalogs, every `Tr` property in `fill_tr`; the Slint
changes checked and rendered before hand-over (guide §1).

## 6. Telemetry and logs

- At open: `info!("audio: capture <name> category=<Other|Communications>
  aec=<tier> ref=<render name> output=<kind>")`.
- `audio stats:` every 10 s carries the `aec:` field (§3.3).
- `telemetry_samples` gets the tier as a small integer when the
  telemetry columns are extended (guide §5, the same change that adds
  the audio rung); until then the log is the record.

## 7. Verification on the users' machines

In order, each one recorded in the guide with machine and result:

1. Laptop on speakers, one far-end talker: they must not hear
   themselves. `/audio` shows `aec: platform(...)`.
2. Same laptop, headphones plugged: `aec: none (headphones)`, no
   ducking of other apps, sound as before.
3. Plug/unplug during a call: the stats line flips within a second,
   no stream restart audible beyond the device change itself.
4. A Windows 10 machine with a USB microphone and no OEM effects:
   expected `aec: none (no effect on endpoint)` — this is the case §8
   exists for; count how many users land here.
5. Windows 11 22H2+: the log shows the reference endpoint accepted
   (`set_echo_cancellation_render_endpoint` succeeded).

An optional `baucord --audio-probe` prints, for every capture endpoint:
form factor of the current output, whether the effects manager
exists, and the effect list with states — one paste from a user
answers tier questions without a call.

## 8. Tier 2: when the endpoint has no AEC

Decided after §7 step 4 has numbers (D7). The two candidates:

| | Voice Capture DSP (Microsoft DMO) | WebRTC AEC3 in-app |
|---|---|---|
| availability | every Windows since Vista, in the box | source build (C++), `webrtc-audio-processing` crate |
| quality | 2000s-era AEC+NS+AGC+VAD; wideband at most (≤ 22.05 kHz mono output — verify), i.e. Opus WB instead of our SWB/FB | state of the art; full band |
| integration | replaces our WASAPI capture in that tier (the DMO in source mode owns both devices); COM + property stores | our capture + a loopback capture of the render endpoint (`AUDCLNT_STREAMFLAGS_LOOPBACK`, which `wasapi` supports) → APM `process_reverse`/`process` per 10 ms; alignment is on us |
| reuse | Windows only | Linux (no PipeWire), Android without platform AEC, macOS if VPIO ever fails |
| build risk | none | MSVC build of the WebRTC APM; binary size |

If fewer than ~1 in 10 users hit step 4, tier 2 waits. If more, the
DMO is the fast route and AEC3 the right one; the spec's §12 records
the pick with the numbers.

## 9. Other platforms (the same contract)

- **macOS**: `kAudioUnitSubType_VoiceProcessingIO` (VPIO) — AEC, NS
  and AGC from the OS, needs a native CoreAudio backend beside
  `wasapi.rs` (cpal's CoreAudio path does not expose the voice unit).
  `OutputKind` from the output device's transport type / data source.
- **Linux**: PipeWire's `module-echo-cancel` (WebRTC-based) does the
  job system-wide when the user enables it; the app's part is the
  Communications-equivalent stream role (`media.role = Communication`)
  so PipeWire can apply it, and reporting whether the echo-cancel
  source is what we opened. Without PipeWire, tier 2's software path.
- **Android**: `Usage::VoiceCommunication` + `InputPreset::VoiceCommunication`
  + `MODE_IN_COMMUNICATION`, as in `ANDROID-SPEC.md` §6; `OutputKind`
  from the current route (speaker vs earpiece/headset). Both streams
  in performance mode *None* and capture on an audio session of its
  own, with an `AcousticEchoCanceler` created on that session and read
  back (E9).
- **iOS**: the VPIO unit in the `voiceChat` mode (`native::vpio`,
  `native::avsession`; E10); `OutputKind` from the session route's
  output ports.

Each reports through the same `AecReport`, so `/audio` reads the same
on every OS.

## 10. Files touched (1.1.5)

`audio/src/lib.rs` (`AecMode`, `OutputKind`, `AecReport`, `wants_aec`
+ tests, `AudioStats`), `audio/src/native/wasapi.rs` (§4),
`client/src/config.rs` (`echo_cancellation`, `logging.level`),
`client/src/voice.rs` (stats line), `desktop/src/app.rs` +
`settingsui.rs` (pickers, status, `fill_tr`), `ui/slint/settings.slint`,
`ui/slint/tr.slint`, the three `.ftl` catalogs, `desktop/src/main.rs`
(live log level), `README.md` (a paragraph under audio settings).

## 11. Risks

| risk | retire by |
|---|---|
| an OEM render chain changes the sound when the category flips | §4.1 last paragraph; render stays `Other` if so |
| Communications routing surprises a user whose "Default Communications Device" is set to something odd | the settings tooltip names the device in use; `/audio` prints it |
| the effects manager reports AEC "present" but the OEM effect is a no-op | step 1 of §7 is the test, not the report |
| double NS (OEM + RNNoise) sounds worse on some laptop | D6; needs a recording to act |
| `windows` crate version drift between `wasapi`, `video` and the new helper | one version, 0.62, checked in `Cargo.lock` |

## 12. Decisions taken after this draft

Implemented 2026-10-06 from this draft (the old branch was not
recovered; only this spec was). Cross-platform in shape, Android and
Windows in substance.

| # | decision | why |
|---|---|---|
| E1 | **The contract is `audio::aec`**, platform-free and host-tested: `AecMode` (serde lowercase, `ALL` order = the picker), `OutputKind`, `wants_aec`, `AecTier { Platform, None }`, `AecReport { mode, tier, output, reference, reason }` with `summary()`, the `reasons` constants, and the two classifiers `android_output_kind(types)` / `windows_output_kind(form_factor)`. `AudioStats.aec` (a mutex: written per (re)open, read every 10 s) plus `aec_generation` for pollers; `VoiceChatConfig.echo_cancellation`; `VoiceChat::set_echo_cancellation` / `aec_report` | one place decides and reports; every backend only says "opened on the call path or not" and "was a canceller there" |
| E2 | **No vendor in the report.** §4.4 assumed `AUDIO_EFFECT` carries the implementer's CLSID; it carries the effect *type* GUID (`AUDIO_EFFECT_TYPE_ACOUSTIC_ECHO_CANCELLATION`, checked in windows 0.62), so Inbox vs OEM cannot be told. The tier is `platform` or `none` | the report must not claim what it cannot know |
| E3 | **`VoiceChat` owns the plan**: `plan_aec()` runs with both streams closed (start, `restart_all`), decides `wants_aec(mode, platform_output_kind())`, puts the platform in the matching state (Android's mode, Windows' watcher role) and hands `voice: bool` to `AudioInput` / `AudioOutput`; `update_aec_report()` after every capture open combines the plan with the backend's verdict. A per-side restart whose plan went stale (headphones plugged) becomes a both-sides reopen (§4.5); on Android, where a rerouted stream need not fail, the 2 s poll checks the output too | the Android mode must be set before streams open and cleared after they close; only the engine sees both sides |
| E4 | **Android first** (§9): `native/aaudio.rs` opens the voice-call streams on `ndk::audio` (usage + input preset `VoiceCommunication`, content type Speech, LowLatency, shared, 48 kHz mono f32) — only when `wants_aec`; otherwise cpal exactly as before. `native/android.rs` (JNI): `MODE_IN_COMMUNICATION` + `setSpeakerphoneOn(output == Speakers)` held by `VoiceChat` while the streams live (restored to the previous mode on drop; Android also resets it if the process dies), output kind from `AudioManager.getDevices(OUTPUTS)` types, verification = the preset stuck **and** `AcousticEchoCanceler.isAvailable()` | cpal cannot set usage or preset, and without both Android never runs its canceller; headphones (incl. Bluetooth, which call mode would drop to SCO) keep the media path |
| E5 | **Windows per §4**, with: the category set on *every* `Initialize` attempt (the 48 kHz retry creates a fresh client); the AEC reference = the user's output device or the default communications render device; the verdict from the effects manager (AEC effect present and `AUDIO_EFFECT_STATE_ON` → platform; none → `no canceller on this device`; no manager, i.e. Windows 10 → platform, unverified); the device watcher follows the Communications role while on the call path; the form factor read through `windows` 0.62 (same version and features `wasapi` already uses) on a short-lived COM thread. **Written against the crate sources, not compiled**: the `windows` crates do not type-check off Windows — the first Windows `cargo check` is the check | as specified; the one surprise (E2) is reporting only |
| E6 | Strings: the picker (Automatic / Always / Off), six status lines keyed off `reasons` (on, headphones, off, no canceller, refused, unsupported) plus "takes effect when you join voice", and the hint per platform — Windows names the ducking (§4.6), Android says the volume buttons become call volume | the status is what tells "no canceller here" from "works" without a log |
| E7 | Still open: `--audio-probe` (§7), the telemetry column (§6), macOS VPIO / Linux PipeWire role (§9), the README paragraph; on Android the screen-share audio is a media stream outside the call path, so the platform may not cancel it (it does follow the call's route: in communication mode Android sends media to the communication device — ANDROID-SPEC.md D31, which also replaced `setSpeakerphoneOn` with `setCommunicationDevice`) | out of this round |
| E8 | **The VPIO backend is planned in IOS-SPEC.md** (2026-10-06, D8 there): `audio/src/native/vpio.rs` on `coreaudio-rs` (already in the tree through cpal) — one `kAudioUnitSubType_VoiceProcessingIO` unit for both directions, 48 kHz mono `f32`, the other-audio ducking set to its minimum on iOS 17+/macOS 14+. Written for the iPhone first (its M3), then the same file with the device chosen through `kAudioOutputUnitProperty_CurrentDevice` is §9's macOS item | one implementation for both Apple platforms; the Mac users on built-in speakers are the desktop's remaining echo case |
| E9 | **Android, 2026-10-07: "Always" did nothing on two users' phones.** The voice-call streams asked for `PerformanceMode::LowLatency` with no session id — exactly what `AAudio` puts on its **MMAP** path where the device has one (most current phones), and MMAP bypasses the platform's effects, the echo canceller included (AOSP `AudioStreamBuilder::build`: MMAP only for low latency, never with a session id). The report said `platform` anyway: the verdict was "the preset stuck and `isAvailable()`", true on such phones. Now both streams use **performance mode None** (the legacy path calls take — no MMAP, no FAST track, so the policy picks the VoIP input/output where the HAL has them), capture is opened with **an allocated session**, and `native/android.rs` creates an **`AcousticEchoCanceler` on that session, `setEnabled(true)`, and reads `getEnabled()`**: on → `platform`; created but off → `none (canceller would not switch on)` with the `setEnabled` status in the log; no effect for the session → `isAvailable()` decides as before (some HALs cancel without exposing an effect). The effect is released with the stream. Cost: a period or two more latency on the call path only (headphones under Automatic keep cpal's low-latency streams); the playout cushion follows the callback size (D24) | an AEC that is not running must not report `platform` (D5); WebRTC's Android audio attaches the effect the same way |
| E10 | **iOS VPIO shipped (2026-10-07)**, as E8 planned: `native/vpio.rs` — one `VoiceProcessingIO` unit, bus 1 the processed microphone, bus 0 the speaker (the reference), 48 kHz mono `f32` client format both ways, max 4096 frames per slice (the locked screen), bypass 0 and read back after `initialize` (the verdict), advanced ducking at its minimum. Both sides must be on the unit or neither (its playback is the canceller's reference), so `plan_aec` opens it before either side starts and holds it for the plan (`vpio::prepare`); if it will not open, both sides take plain streams and the report says `none (voice-call path refused)`. The sides still open on their own and attach to the running unit (a process-wide `Weak`): each hands its ring through a one-slot mailbox the callback takes with `try_lock`, a dropped side flags its half off, and a half the callback lets go of goes back through a channel to be freed off the I/O thread — so either side can be reopened alone. `native/avsession.rs`: the session's mode (`voiceChat` on the call path, `default` otherwise, set in `plan_aec` with both streams closed), 48 kHz / 10 ms preferences, the route's output ports → `OutputKind` (`aec::ios_output_kind`, host-tested), the loudspeaker kept if the mode change moved the call to the earpiece, and the interruption-ended / media-services-reset notifications → both stream-failed flags → `service()` reopens **both** sides (`SHARED_VOICE_UNIT`). Not wanted (headphones under Automatic, or Off) → cpal's `RemoteIO` as before. The capture and render halves are shared with Android's backend (`native/pump.rs`). The default-device poll is skipped on iOS as on Android (routing is the session's). Known gaps: a watched stream's sound plays through its own `RemoteIO` (`StreamAudioOutput`), outside the unit's reference — not cancelled, and ducked a little (Android's E7 limit, the same shape); an interruption that never posts *ended* leaves voice stopped until the next reopen; the verdict reads back the bypass property, which says processing is on, not that it is cancelling | the iPhone's "aec: none (not on this platform yet)" was accurate until now |
| E11 | **Asking Windows not to duck other apps** (2026-10-07; `native/wasapi.rs` `ask_not_to_duck`, `third_party/wasapi`). Users on 1.1.7 reported every other app much quieter while baucord was up: §4.6's accepted side effect, Windows' default "reduce other sounds by 80 %" on any Communications stream, was not acceptable in practice. Now each voice-path stream, after `Initialize` and before `Start`, asks for `AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS` through `IAudioClientDuckingControl` (Windows 10 build 20348+ / Windows 11), and the stream's open line says what Windows answered: `do-not-duck=requested`, `unavailable` (no such service: older Windows, or a capture stream if Windows offers it for render only) or `refused`. `wasapi` 0.24/0.25 keep the `IAudioClient` private, so the crate is vendored with one accessor (`AudioClient::as_iaudioclient`, BAUCORD-PATCH.md there) behind `[patch.crates-io]`. Not yet known: whether it stops ducking caused by the capture stream — Microsoft documents it for render streams, and `IAudioSessionControl2::SetDuckingPreference` is no help (it protects the app being lowered, not the one lowering; a calling app that sets it still ducks others). Windows 10 keeps ducking either way. If Windows 11 still ducks with `requested` on both lines, the remaining fixes are the software canceller (§8: no Communications stream at all) or Auto leaving speakers off the call path; the user's own fix is Sound settings → Communications → "Do nothing" (`control mmsys.cpl,,3`) | the reports came from people who keep a game or music playing beside the call — the case §4.6 underrated |
| E12 | **A watched stream on a phone's call path is part of the reference** (2026-10-08; ANDROID-SPEC.md D56, IOS-SPEC.md D52): it is mixed into the voice output, so the platform canceller removes it from the microphone like the voice — E7's "screen-share audio is a media stream outside the voice path" no longer holds there. The desktop keeps the stream on its own output (Windows' canceller references the whole endpoint) |
