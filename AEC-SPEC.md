# baucord — Acoustic echo cancellation (AEC) technical spec

> **Status (2026-10-09):** in use on Windows (the Communications
> category, §4), Android (voice-call streams with
> `AcousticEchoCanceler`, E9) and iOS (the VPIO unit, E10); on the
> phones a watched stream is cancelled with the voice (E12). Open:
> macOS and Linux (§9, PRODUCTION-PLAN PRD-5), a software canceller
> where the platform has none (§8), Windows' ducking on Windows 10
> (E11, E14, E15, CLI-11). Decisions in §1 are the first draft's; §12 collects
> the ones taken since, and wins where they differ. Start from
> `START-HERE.md`.

## 0. Purpose and scope

People on laptops using the built-in speakers and microphone send
everyone else their own voices back. Until 1.1.7 baucord had no echo
cancellation — not in software, and not from the OS, because of one
omission (§2). The design is **platform-first**: every OS ships a
tuned canceller that already has the hard part solved — the reference
signal, i.e. knowing exactly what the speakers played and when — so
the app asks the platform first and would carry its own canceller
only where the platform has none (§8).

Windows came first (§4), because it is where the users are. The
contract (§3) is what every platform implements: Android's
`VoiceCommunication` path (`ANDROID-SPEC.md` §6) and iOS's VPIO unit
(`IOS-SPEC.md` §6.3) are its second and third; macOS and Linux are
§9.

Out of scope: noise suppression and AGC beyond what the platform chain
does (RNNoise stays as it is, §3.4), any change to the encoder
ladder.

## 1. Decisions

| # | decision | why |
|---|---|---|
| D1 | Platform AEC first; software AEC only as a measured fallback | the platform has the reference signal and the tuning; we have neither |
| D2 | One setting, `audio.echo_cancellation`: **Auto** (default) / Always / Off | Auto = only when the output is speakers; nobody on headphones sees any change |
| D3 | In AEC mode the streams are opened in Windows' **Communications** category; otherwise the category stays as today (`Other`) | that category is what turns the OS voice chain on — and what triggers Windows' "reduce other sounds" ducking, which headphone users must not get |
| D4 | In AEC mode "Default" resolves to the **Default Communications Device**; otherwise the console default, as today (since 1.2.1 the Communications default unless the setting is Off, for the decision too: E13) | it is the device Windows users assign to calls; Discord and Teams follow it |
| D5 | The active tier is *verified*, not assumed, and reported in `/audio`, the log and telemetry | an AEC that isn't there is the next bug report; the report is what tells tier 2 apart from "works" |
| D6 | RNNoise stays on by default; the setting is unchanged | the OEM chain's own NS is device-dependent; removing ours on faith would regress quiet-room users |
| D7 | The fallback (§8) is chosen after the tier reports from the users' machines, not now | two candidates with different costs; the probe data decides |

## 2. Why Windows had none

Windows applies its voice-processing chain — the OEM's APOs (Realtek,
Intel, Synaptics, Cirrus ship AEC/NS/AGC on nearly every laptop) and,
on Windows 11, Microsoft's own where the driver opted in — **only to
streams in the `Communications` category**. `wasapi.rs` used to open
its streams without a category (`AudioCategory_Other`), so no
canceller ever ran on our microphone, whatever the laptop had. Headset
users never heard the echo, which is why it was tolerable.

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

`AudioStats.aec` (E1):

```rust
pub struct AecReport {
    pub mode: AecMode,
    pub tier: AecTier,               // Platform, None (a software tier would join it, §8)
    pub output: OutputKind,
    pub reference: Option<String>,   // the render endpoint used as reference, if known
    pub reason: &'static str,        // why this tier: `aec::reasons` ("headphones", "off", …)
}
```

No vendor: Windows cannot tell an inbox canceller from an OEM one
(E2). Filled when the streams open, refreshed on every reopen.
`voice.stats_summary()` appends `aec: platform (speakers)` /
`aec: none (headphones)` / `aec: none (no canceller on this device)`
to the 10-second `audio stats:` line, and the settings page shows the
same words under the picker so the user can see what they got.

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
       Ok(Some(effects)) if effects has ACOUSTIC_ECHO_CANCELLATION on
                          => Platform
       Ok(_)              => None ("no canceller on this device")
       Err(_)             => Platform, unverified, if the category was set
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

Since 1.2.1 (E13): the Communications default unless echo cancellation
is Off, then the Console one (`communications_default`). That one
answer is used for the Automatic decision, for both streams on or off
the call path, and for the device watcher. The Sound panel shows these
as two separate "Default" entries; the settings page's device list
keeps one "Default" row and the tooltip says which one applies.

### 4.4 Vendor

Dropped (E2): `AUDIO_EFFECT` carries the effect's *type*, not who
implemented it, so the report says `platform` and nothing more.

### 4.5 Device and setting changes

The existing default-device and device-loss paths reopen the streams;
the open sequence re-evaluates `wants_aec` from the *new* render
endpoint every time, so plugging in headphones drops out of AEC mode
and unplugging them re-enters it, with the stats line saying so. An
output change that moves the plan or the reference reopens both sides
(`VoiceChat::reconcile`, a re-plan; E13); there is no in-place
reference update.

### 4.6 Ducking

A Communications session makes Windows apply the user's "When Windows
detects communications activity" setting (default: reduce other sounds
by 80 %). Auto confines this to speaker users. The setting's help
string says it plainly, in all three catalogs: "On speakers, Windows
may lower other apps while you talk (Sound settings →
Communications)." Since E11 each voice stream also asks Windows not to
duck others (`IAudioClientDuckingControl`, Windows 11); the stream's
log line says whether it was granted. Users still ducked: Sound
settings → Communications → "Do nothing" (`control mmsys.cpl,,3`) —
which Settings → Audio now says, with a button, to exactly the users
Windows would not take the request from (E15).

What is and is not covered (E14, read from the code on 2026-10-12;
none of it run on Windows):

| Who | Streams | Does Windows lower other apps? |
|---|---|---|
| Echo cancellation Off | plain, console default or the picked device | no: nothing says "call" |
| Automatic on headphones, a device picked | plain, by name | no |
| Automatic on headphones, "Default" | plain, the Communications default **opened by its id** | no — since E14; 1.2.1 as first written opened it through the Communications role, which is a call to Windows whatever the category |
| Automatic on speakers / unknown, or Always — Windows 11 | Communications category | render asks not to and Windows documents that it works; capture asks too, and whether Windows offers it there is not known |
| the same — Windows 10 | Communications category | **yes**: the opt-out does not exist before build 20348 |

"Speakers" is what most desktop jacks report with headphones plugged
in, and unknown counts as speakers, so the last two rows are most
desktop users, headphones or not. On headphones, Echo cancellation →
Off is the in-app way out.

Our own sound is a separate matter: Windows lowers every session that
is not part of a call, baucord's included — a watched stream on
another endpoint than the voice, or everything when the call is
another app's. Each render stream therefore sets the session's
ducking preference (`IAudioSessionControl2::SetDuckingPreference`,
every Windows), and a watched stream's own output does the same for
its endpoint. The render line logs `own-volume=` and what the user's
Windows setting is (`Windows during calls:`).

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
   themselves. `/audio` shows `aec: platform (speakers)`.
2. Same laptop, headphones plugged: `aec: none (headphones)`, no
   ducking of other apps, sound as before.
3. Plug/unplug during a call: the stats line flips within a second,
   no stream restart audible beyond the device change itself.
4. A Windows 10 machine with a USB microphone and no OEM effects:
   expected `aec: none (no canceller on this device)` — this is the case §8
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

- **Android** (done, E4, E9): `Usage::VoiceCommunication` +
  `InputPreset::VoiceCommunication` + `MODE_IN_COMMUNICATION`, as in
  `ANDROID-SPEC.md` §6; `OutputKind` from the current route (speaker
  vs earpiece/headset). Both streams in performance mode *None* and
  capture on an audio session of its own, with an
  `AcousticEchoCanceler` created on that session and read back.
- **iOS** (done, E10): the VPIO unit in the `voiceChat` mode
  (`native::vpio`, `native::avsession`); `OutputKind` from the session
  route's output ports.
- **macOS** (open, PRD-5): the same VPIO unit, with the device chosen
  through `kAudioOutputUnitProperty_CurrentDevice` (E8) — cpal's
  CoreAudio path does not expose the voice unit. `OutputKind` from the
  output device's transport type / data source.
- **Linux** (open, PRD-5): PipeWire's `module-echo-cancel`
  (WebRTC-based) does the job system-wide when the user enables it;
  the app's part is the Communications-equivalent stream role
  (`media.role = Communication`) so PipeWire can apply it, and
  reporting whether the echo-cancel source is what we opened. Without
  PipeWire, tier 2's software path.

Each reports through the same `AecReport`, so `/audio` reads the same
on every OS.

## 10. Where it lives

`audio/src/aec.rs` (the contract and its tests: `AecMode`,
`OutputKind`, `wants_aec`, `AecReport`, the output-kind classifiers),
`audio/src/lib.rs` (`VoiceChat::plan_aec`, the report), the backends
in `audio/src/native/` (`wasapi.rs`, `aaudio.rs` + `android.rs`,
`vpio.rs` + `avsession.rs`), `client/src/config.rs`
(`echo_cancellation`), `client/src/voice.rs` (the stats line),
`frontend/src/settingsui.rs` and `ui/slint/settings.slint` (the picker
and its status line).

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
| E3 | **`VoiceChat` owns the plan**: `plan_aec()` runs with both streams closed (start and every re-plan: `plan_and_open`, review P2-55), decides `wants_aec(mode, platform_output_kind())`, puts the platform in the matching state (Android's mode, Windows' watcher role) and hands `voice: bool` to `AudioInput` / `AudioOutput`; `update_aec_report()` after every capture open combines the plan with the backend's verdict. A per-side restart whose plan went stale (headphones plugged) becomes a both-sides reopen (§4.5); on Android, where a rerouted stream need not fail, the 2 s poll checks the output too | the Android mode must be set before streams open and cleared after they close; only the engine sees both sides |
| E4 | **Android first** (§9): `native/aaudio.rs` opens the voice-call streams on `ndk::audio` (usage + input preset `VoiceCommunication`, content type Speech, LowLatency, shared, 48 kHz mono f32) — only when `wants_aec`; otherwise cpal exactly as before. `native/android.rs` (JNI): `MODE_IN_COMMUNICATION` + `setSpeakerphoneOn(output == Speakers)` held by `VoiceChat` while the streams live (restored to the previous mode on drop; Android also resets it if the process dies), output kind from `AudioManager.getDevices(OUTPUTS)` types, verification = the preset stuck **and** `AcousticEchoCanceler.isAvailable()` | cpal cannot set usage or preset, and without both Android never runs its canceller; headphones (incl. Bluetooth, which call mode would drop to SCO) keep the media path |
| E5 | **Windows per §4**, with: the category set on *every* `Initialize` attempt (the 48 kHz retry creates a fresh client); the AEC reference = the user's output device or the default communications render device; the verdict from the effects manager (AEC effect present and `AUDIO_EFFECT_STATE_ON` → platform; none → `no canceller on this device`; no manager, i.e. Windows 10 → platform, unverified); the device watcher follows the Communications role while on the call path; the form factor read through `windows` 0.62 (same version and features `wasapi` already uses) on a short-lived COM thread. **Written against the crate sources, not compiled**: the `windows` crates do not type-check off Windows — the first Windows `cargo check` is the check | as specified; the one surprise (E2) is reporting only |
| E6 | Strings: the picker (Automatic / Always / Off), six status lines keyed off `reasons` (on, headphones, off, no canceller, refused, unsupported) plus "takes effect when you join voice", and the hint per platform — Windows names the ducking (§4.6), Android says the volume buttons become call volume | the status is what tells "no canceller here" from "works" without a log |
| E7 | Still open: `--audio-probe` (§7), the telemetry column (§6), macOS VPIO / Linux PipeWire role (§9), the README paragraph; on Android the screen-share audio is a media stream outside the call path, so the platform may not cancel it (it does follow the call's route: in communication mode Android sends media to the communication device — ANDROID-SPEC.md D31, which also replaced `setSpeakerphoneOn` with `setCommunicationDevice`) | out of this round |
| E8 | **The VPIO backend is planned in IOS-SPEC.md** (2026-10-06, D8 there): `audio/src/native/vpio.rs` on `coreaudio-rs` (already in the tree through cpal) — one `kAudioUnitSubType_VoiceProcessingIO` unit for both directions, 48 kHz mono `f32`, the other-audio ducking set to its minimum on iOS 17+/macOS 14+. Written for the iPhone first (its M3), then the same file with the device chosen through `kAudioOutputUnitProperty_CurrentDevice` is §9's macOS item | one implementation for both Apple platforms; the Mac users on built-in speakers are the desktop's remaining echo case |
| E9 | **Android, 2026-10-07: "Always" did nothing on two users' phones.** The voice-call streams asked for `PerformanceMode::LowLatency` with no session id — exactly what `AAudio` puts on its **MMAP** path where the device has one (most current phones), and MMAP bypasses the platform's effects, the echo canceller included (AOSP `AudioStreamBuilder::build`: MMAP only for low latency, never with a session id). The report said `platform` anyway: the verdict was "the preset stuck and `isAvailable()`", true on such phones. Now both streams use **performance mode None** (the legacy path calls take — no MMAP, no FAST track, so the policy picks the VoIP input/output where the HAL has them), capture is opened with **an allocated session**, and `native/android.rs` creates an **`AcousticEchoCanceler` on that session, `setEnabled(true)`, and reads `getEnabled()`**: on → `platform`; created but off → `none (canceller would not switch on)` with the `setEnabled` status in the log; no effect for the session → `isAvailable()` decides as before (some HALs cancel without exposing an effect). The effect is released with the stream. Cost: a period or two more latency on the call path only (headphones under Automatic keep cpal's low-latency streams); the playout cushion follows the callback size (D24) | an AEC that is not running must not report `platform` (D5); WebRTC's Android audio attaches the effect the same way |
| E10 | **iOS VPIO shipped (2026-10-07)**, as E8 planned: `native/vpio.rs` — one `VoiceProcessingIO` unit, bus 1 the processed microphone, bus 0 the speaker (the reference), 48 kHz mono `f32` client format both ways, max 4096 frames per slice (the locked screen), bypass 0 and read back after `initialize` (the verdict), advanced ducking at its minimum. Both sides must be on the unit or neither (its playback is the canceller's reference), so `plan_aec` opens it before either side starts and holds it for the plan (`vpio::prepare`); if it will not open, both sides take plain streams and the report says `none (voice-call path refused)`. The sides still open on their own and attach to the running unit (a process-wide `Weak`): each hands its ring through a one-slot mailbox the callback takes with `try_lock`, a dropped side flags its half off, and a half the callback lets go of goes back through a channel to be freed off the I/O thread — so either side can be reopened alone. `native/avsession.rs`: the session's mode (`voiceChat` on the call path, `default` otherwise, set in `plan_aec` with both streams closed), 48 kHz / 10 ms preferences, the route's output ports → `OutputKind` (`aec::ios_output_kind`, host-tested), the loudspeaker kept if the mode change moved the call to the earpiece, and the interruption-ended / media-services-reset notifications → both stream-failed flags → `service()` reopens **both** sides (`SHARED_VOICE_UNIT`). Not wanted (headphones under Automatic, or Off) → cpal's `RemoteIO` as before. The capture and render halves are shared with Android's backend (`native/pump.rs`). The default-device poll is skipped on iOS as on Android (routing is the session's). Known gaps: a watched stream's sound plays through its own `RemoteIO` (`StreamAudioOutput`), outside the unit's reference — not cancelled, and ducked a little (Android's E7 limit, the same shape); an interruption that never posts *ended* leaves voice stopped until the next reopen; the verdict reads back the bypass property, which says processing is on, not that it is cancelling | the iPhone's "aec: none (not on this platform yet)" was accurate until now |
| E11 | **Asking Windows not to duck other apps** (2026-10-07; `native/wasapi.rs` `ask_not_to_duck`, `third_party/wasapi`). Users on 1.1.7 reported every other app much quieter while baucord was up: §4.6's accepted side effect, Windows' default "reduce other sounds by 80 %" on any Communications stream, was not acceptable in practice. Now each voice-path stream, after `Initialize` and before `Start`, asks for `AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS` through `IAudioClientDuckingControl` (Windows 10 build 20348+ / Windows 11), and the stream's open line says what Windows answered: `do-not-duck=requested`, `unavailable` (no such service: older Windows, or a capture stream if Windows offers it for render only) or `refused`. `wasapi` 0.24/0.25 keep the `IAudioClient` private, so the crate is vendored with one accessor (`AudioClient::as_iaudioclient`, BAUCORD-PATCH.md there) behind `[patch.crates-io]`. Not yet known: whether it stops ducking caused by the capture stream — Microsoft documents it for render streams, and `IAudioSessionControl2::SetDuckingPreference` is no help (it protects the app being lowered, not the one lowering; a calling app that sets it still ducks others). Windows 10 keeps ducking either way. If Windows 11 still ducks with `requested` on both lines, the remaining fixes are the software canceller (§8: no Communications stream at all) or Auto leaving speakers off the call path; the user's own fix is Sound settings → Communications → "Do nothing" (`control mmsys.cpl,,3`) | the reports came from people who keep a game or music playing beside the call — the case §4.6 underrated |
| E12 | **A watched stream on a phone's call path is part of the reference** (2026-10-08; ANDROID-SPEC.md D56, IOS-SPEC.md D52): it is mixed into the voice output, so the platform canceller removes it from the microphone like the voice — E7's "screen-share audio is a media stream outside the voice path" no longer holds there. The desktop keeps the stream on its own output (Windows' canceller references the whole endpoint) |
| E13 | **The output and "Default" move both sides** (2026-10-10, 1.2.1, review P1-18; `audio/src/lib.rs` `set_output_device`, `communications_default`, `native/wasapi.rs`). A new output only reopened playback, so capture kept the old reference and the old plan: picking headphones left the canceller on (and the speakers' reference), picking speakers after headphones left it off. Now an output change reopens both sides when the plan goes stale or the reference changes, and a capture with no output picked passes no reference, so Windows follows the Communications default itself. "Default" is resolved once from the setting: the Communications device unless echo cancellation is Off, for Automatic's decision, both streams and the device watcher alike; the report names a reference only when the reference control took it (D5); the fallback default poll is skipped while "Default" means the Communications device (it reopened the streams every 2 s); switching the setting to or from Off restarts the streams that follow a default. **Not compiled here** (Windows code) | reading only the decision from the Communications default would have opened the plain streams on the Console one — with a headset as the Communications default and speakers as the Console one, Automatic says headphones and everyone gets echo. The cost: someone whose two defaults differ now hears voice on the Communications device under Automatic and Always |
| E14 | **A plain stream is not a call, and our own sound is not lowered** (2026-10-12, 1.2.1; `native/wasapi.rs` `stream_device`, `keep_own_volume`, `keep_own_volume_on`, `windows_ducking_setting`; `stream/output.rs`). Asked "is ducking really off on Windows?" with users on 1.2.0 still reporting a volume drop, the code was read end to end (§4.6 has the table). Three gaps. (1) E13 made "Default" the Communications default for plain streams too, fetched with `GetDefaultAudioEndpoint(eCommunications)`: Windows treats any stream opened on that device object as communications activity, so headphone users on "Default" would have started ducking in 1.2.1, with no opt-out asked (it was asked on the call path only). A plain stream now takes that device's id and opens it with `GetDevice`, which carries no role; the opt-out is asked on every stream regardless. (2) Nothing protected baucord's own sessions: the render stream now sets `SetDuckingPreference(TRUE)` on its session (shared by a watched stream's cpal output on the same endpoint), and the stream output asks for every session of ours on its endpoint. (3) Nothing in a log said what Windows was set to do: the render line now ends with the user's Sound → Communications setting (`HKCU\Software\Microsoft\Multimedia\Audio\UserDuckingPreference`; absent = 80 %). **Not changed, and not fixable from here:** Windows 10 on the call path still ducks other apps. **Type-checked for Windows, never run** | the role tag is how Chrome turns ducking *on*; D3 promised headphone users would not get it |
| E15 | **Windows 10 on speakers: say it, and point at the way out** (2026-10-12, 1.2.1; `AecReport.lowers_others`, `aec::system_lowers_others`, `AudioOutput::lowers_others`, `native/wasapi.rs` `NativeRender.keeps_others`; frontend `tell_lowering_once`, `host::open_sound_settings`, `settings.slint`). Windows before 11 has no "do not duck" request, and the platform canceller only runs on Communications streams, so there the choice is ducking or echo; ending both takes our own canceller on plain streams (§8), which is not being built now. Decided: keep the canceller and tell the user. The playback stream's answer to E11's request goes into the report (`lowers_others`: on the call path and not `requested` — whatever the canceller's verdict, a stream with no canceller behind it is as much a call to Windows); `AecReport::lowering_others` adds the user's own Windows setting, read fresh. While both hold, Settings → Audio shows a notice under the status line with a button that opens `control.exe mmsys.cpl,,3` (the Sound dialog on its Communications tab), and it disappears by itself when the user picks "Do nothing". Once per install (`audio.lowering_told` in settings.json) a toast and an activity line point there, from the 2.5 s device poll, when the main window is on screen. A cpal fallback output reports nothing (it is a plain stream), and a Windows 11 where only capture was refused the request is not flagged: whether that ducks is still unknown (E11). **Type-checked for Windows, never run; the notice was rendered in the three languages** | the reports keep coming from people who do not know it is Windows doing it |
