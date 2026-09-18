# baucord — Acoustic echo cancellation (AEC) technical spec

Status: draft 1, 2026-09-15. Ships in 1.1.5 with the other items in
`SESSION-GUIDE.md` §5. Decisions in §1 are taken; §12 collects the
ones taken after this draft, the way `PERSISTENCE-SPEC.md` §12 does.

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
Communications)."

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
  from the current route (speaker vs earpiece/headset).

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

(none yet)
